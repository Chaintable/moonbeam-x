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

//! `trace_filter` and `trace_debankBlock` RPC handlers and associated service tasks.
//! The RPC handler rely on `CacheTask` which provides a future that must be run inside a tokio
//! executor.
//!
//! The implementation is composed of multiple tasks :
//! - Many calls the RPC handler `Trace::filter`, communicating with the main task.
//! - A main `CacheTask` managing the cache and the communication between tasks.
//! - For each traced block an async task responsible to wait for a permit, spawn a blocking
//!   task and waiting for the result, then send it to the main `CacheTask`.

pub mod debank;

use futures::{select, FutureExt};
use std::{
	collections::{BTreeMap, HashMap},
	future::Future,
	marker::PhantomData,
	sync::Arc,
	time::{Duration, Instant},
};
use tokio::{
	sync::{mpsc, oneshot, Semaphore},
	time::interval,
};
use tracing::{instrument, Instrument};

use sc_client_api::backend::{Backend, StateBackend, StorageProvider};
use sc_service::SpawnTaskHandle;
use sp_api::{ApiExt, Core, ProvideRuntimeApi};
use sp_block_builder::BlockBuilder;
use sp_blockchain::{
	Backend as BlockchainBackend, Error as BlockChainError, HeaderBackend, HeaderMetadata,
};
use sp_core::keccak_256;
use sp_runtime::traits::{BlakeTwo256, Block as BlockT, Header as HeaderT};
use substrate_prometheus_endpoint::Registry as PrometheusRegistry;

use ethereum_types::{H160, H256};
use fc_rpc::{lru_cache::LRUCacheByteLimited, Eth, EthConfig};
use fc_rpc_core::types::{BlockNumberOrHash, BlockTransactions, Receipt, RichBlock, Transaction};
use fc_storage::StorageOverride;
use fp_rpc::EthereumRuntimeRPCApi;
use jsonrpsee::core::RpcResult;

/// Trait for providing block data from EthApi.
/// This abstracts over EthApi to allow Trace to get data without complex generics.
#[async_trait::async_trait]
pub trait EthDataProvider: Send + Sync {
	/// Get all transaction receipts for a block.
	async fn block_transaction_receipts(
		&self,
		number_or_hash: BlockNumberOrHash,
	) -> RpcResult<Option<Vec<Receipt>>>;

	/// Get block with full transaction details.
	async fn block_by_number(
		&self,
		number_or_hash: BlockNumberOrHash,
		full: bool,
	) -> RpcResult<Option<RichBlock>>;
}

use moonbeam_client_evm_tracing::{
	formatters::ResponseFormatter,
	types::block::{self, TransactionTrace},
};
pub use moonbeam_rpc_core_trace::{FilterRequest, TraceServer};
use moonbeam_rpc_core_types::{
	debank::{
		BlockFile, DebankBlock, DebankBlockHeader, DebankEvent as RpcDebankEvent, DebankOutput,
		DebankTrace as RpcDebankTrace, DebankTransaction,
	},
	RequestBlockId, RequestBlockTag,
};
use moonbeam_rpc_primitives_debug::DebugRuntimeApi;

// Import Debank listener and formatter for direct tracing
use moonbeam_client_evm_tracing::{
	formatters::debank::Formatter as DebankFormatter, listeners::debank::Listener as DebankListener,
};

/// Internal type for trace results from blocking tasks
type TxsTraceRes = Result<Vec<TransactionTrace>, String>;

/// Type for trace results sent to requesters (Arc-wrapped for zero-copy sharing)
/// Both success (traces) and error (message) are Arc-wrapped to avoid cloning
/// when multiple waiters are waiting for the same block.
type SharedTxsTraceRes = Result<Arc<Vec<TransactionTrace>>, Arc<String>>;

/// Log target for trace cache operations
const CACHE_LOG_TARGET: &str = "trace-cache";

/// Maximum time allowed for tracing a single block.
const TRACING_TIMEOUT_SECS: u64 = 60;

/// RPC handler. Will communicate with a `CacheTask` through a `CacheRequester`.
pub struct Trace<B, C, BE> {
	_phantom: PhantomData<(B, BE)>,
	client: Arc<C>,
	backend: Arc<BE>,
	frontier_backend: Arc<dyn fc_api::Backend<B>>,
	overrides: Arc<dyn StorageOverride<B>>,
	eth_data_provider: Arc<dyn EthDataProvider>,
	requester: CacheRequester,
	max_count: u32,
	max_block_range: u32,
}

impl<B, C, BE> Clone for Trace<B, C, BE> {
	fn clone(&self) -> Self {
		Self {
			_phantom: PhantomData,
			client: Arc::clone(&self.client),
			backend: Arc::clone(&self.backend),
			frontier_backend: Arc::clone(&self.frontier_backend),
			overrides: Arc::clone(&self.overrides),
			eth_data_provider: Arc::clone(&self.eth_data_provider),
			requester: self.requester.clone(),
			max_count: self.max_count,
			max_block_range: self.max_block_range,
		}
	}
}

impl<B, C, BE> Trace<B, C, BE>
where
	B: BlockT<Hash = H256> + Send + Sync + 'static,
	B::Header: HeaderT<Number = u32>,
	C: HeaderMetadata<B, Error = BlockChainError> + HeaderBackend<B>,
	C: Send + Sync + 'static,
	BE: Send + Sync + 'static,
{
	/// Create a new RPC handler.
	pub fn new(
		client: Arc<C>,
		backend: Arc<BE>,
		frontier_backend: Arc<dyn fc_api::Backend<B>>,
		overrides: Arc<dyn StorageOverride<B>>,
		eth_data_provider: Arc<dyn EthDataProvider>,
		requester: CacheRequester,
		max_count: u32,
		max_block_range: u32,
	) -> Self {
		Self {
			client,
			backend,
			frontier_backend,
			overrides,
			eth_data_provider,
			requester,
			max_count,
			max_block_range,
			_phantom: PhantomData,
		}
	}

	/// Convert an optional block ID (number or tag) to a block height.
	fn block_id(&self, id: Option<RequestBlockId>) -> Result<u32, &'static str> {
		match id {
			Some(RequestBlockId::Number(n)) => Ok(n),
			None | Some(RequestBlockId::Tag(RequestBlockTag::Latest)) => {
				Ok(self.client.info().best_number)
			}
			Some(RequestBlockId::Tag(RequestBlockTag::Earliest)) => Ok(0),
			Some(RequestBlockId::Tag(RequestBlockTag::Finalized)) => {
				Ok(self.client.info().finalized_number)
			}
			Some(RequestBlockId::Tag(RequestBlockTag::Pending)) => {
				Err("'pending' is not supported")
			}
			Some(RequestBlockId::Hash(_)) => Err("Block hash not supported"),
		}
	}

	/// `trace_filter` endpoint (wrapped in the trait implementation with futures compatibility)
	async fn filter(self, req: FilterRequest) -> TxsTraceRes {
		let from_block = self.block_id(req.from_block)?;
		let to_block = self.block_id(req.to_block)?;

		// Validate block range to prevent abuse
		let block_range = to_block.saturating_sub(from_block);
		if block_range > self.max_block_range {
			return Err(format!(
				"block range is too wide (maximum {})",
				self.max_block_range
			));
		}

		let block_heights = from_block..=to_block;

		let count = req.count.unwrap_or(self.max_count);
		if count > self.max_count {
			return Err(format!(
				"count ({}) can't be greater than maximum ({})",
				count, self.max_count
			));
		}

		// Build a list of all the Substrate block hashes that need to be traced.
		let mut block_hashes = vec![];
		for block_height in block_heights {
			if block_height == 0 {
				continue; // no traces for genesis block.
			}

			let block_hash = self
				.client
				.hash(block_height)
				.map_err(|e| {
					format!(
						"Error when fetching block {} header : {:?}",
						block_height, e
					)
				})?
				.ok_or_else(|| format!("Block with height {} don't exist", block_height))?;

			block_hashes.push(block_hash);
		}

		// Fetch traces for all blocks
		self.fetch_traces(req, &block_hashes, count as usize).await
	}

	async fn fetch_traces(
		&self,
		req: FilterRequest,
		block_hashes: &[H256],
		count: usize,
	) -> TxsTraceRes {
		let from_address = req.from_address.unwrap_or_default();
		let to_address = req.to_address.unwrap_or_default();

		let mut traces_amount: i64 = -(req.after.unwrap_or(0) as i64);
		let mut traces = vec![];

		for &block_hash in block_hashes {
			// Request the traces of this block to the cache service.
			// This will resolve quickly if the block is already cached, or wait until the block
			// has finished tracing.
			let block_traces = self
				.requester
				.get_traces(block_hash)
				.await
				.map_err(|arc_error| (*arc_error).clone())?;

			// Filter addresses.
			let mut block_traces: Vec<_> = block_traces
				.iter()
				.filter(|trace| match trace.action {
					block::TransactionTraceAction::Call { from, to, .. } => {
						(from_address.is_empty() || from_address.contains(&from))
							&& (to_address.is_empty() || to_address.contains(&to))
					}
					block::TransactionTraceAction::Create { from, .. } => {
						(from_address.is_empty() || from_address.contains(&from))
							&& to_address.is_empty()
					}
					block::TransactionTraceAction::Suicide { address, .. } => {
						(from_address.is_empty() || from_address.contains(&address))
							&& to_address.is_empty()
					}
				})
				.cloned()
				.collect();

			// Don't insert anything if we're still before "after"
			traces_amount += block_traces.len() as i64;
			if traces_amount > 0 {
				let traces_amount = traces_amount as usize;
				// If the current Vec of traces is across the "after" marker,
				// we skip some elements of it.
				if traces_amount < block_traces.len() {
					let skip = block_traces.len() - traces_amount;
					block_traces = block_traces.into_iter().skip(skip).collect();
				}

				traces.append(&mut block_traces);

				// If we go over "count" (the limit), we trim and exit the loop,
				// unless we used the default maximum, in which case we return an error.
				if traces_amount >= count {
					if req.count.is_none() {
						return Err(format!(
							"the amount of traces goes over the maximum ({}), please use 'after' \
							and 'count' in your request",
							self.max_count
						));
					}

					traces = traces.into_iter().take(count).collect();
					break;
				}
			}
		}

		Ok(traces)
	}
}

impl<B, C, BE> Trace<B, C, BE>
where
	BE: Backend<B> + 'static,
	BE::State: StateBackend<BlakeTwo256>,
	B: BlockT<Hash = H256> + Send + Sync + 'static,
	B::Header: HeaderT<Number = u32>,
	C: ProvideRuntimeApi<B>,
	C: StorageProvider<B, BE>,
	C: HeaderMetadata<B, Error = BlockChainError> + HeaderBackend<B>,
	C: Send + Sync + 'static,
	C::Api: BlockBuilder<B>,
	C::Api: DebugRuntimeApi<B>,
	C::Api: EthereumRuntimeRPCApi<B>,
	C::Api: ApiExt<B>,
{
	/// Implementation of trace_debankBlock.
	async fn trace_debank_block(self, block_id: RequestBlockId) -> Result<DebankOutput, String> {
		// Resolve block ID to (substrate_hash, block_height)
		let (substrate_hash, block_height) = match block_id {
			RequestBlockId::Hash(eth_hash) => {
				// Use frontier backend to map Ethereum block hash → substrate hash
				let substrate_hash =
					fc_rpc::frontier_backend_client::load_hash::<B, C>(
						self.client.as_ref(),
						self.frontier_backend.as_ref(),
						eth_hash,
					)
					.await
					.map_err(|e| format!("Failed to load hash: {:?}", e))?
					.ok_or_else(|| {
						format!("Block with eth hash {:?} not found", eth_hash)
					})?;
				let number = *self
					.client
					.header(substrate_hash)
					.map_err(|e| format!("Failed to get header: {:?}", e))?
					.ok_or_else(|| {
						format!("Block header not found for hash {:?}", substrate_hash)
					})?
					.number();
				(substrate_hash, number)
			}
			_ => {
				let number = match block_id {
					RequestBlockId::Number(n) => n,
					RequestBlockId::Tag(RequestBlockTag::Latest) => self.client.info().best_number,
					RequestBlockId::Tag(RequestBlockTag::Earliest) => 0,
					RequestBlockId::Tag(RequestBlockTag::Finalized) => {
						self.client.info().finalized_number
					}
					RequestBlockId::Tag(RequestBlockTag::Pending) => {
						return Err("'pending' is not supported".to_string());
					}
					RequestBlockId::Hash(_) => unreachable!(),
				};
				let hash = self
					.client
					.hash(number)
					.map_err(|e| {
						format!("Error when fetching block {} hash: {:?}", number, e)
					})?
					.ok_or_else(|| format!("Block with height {} doesn't exist", number))?;
				(hash, number)
			}
		};

		// Get Ethereum block data
		let eth_block = self
			.overrides
			.current_block(substrate_hash)
			.ok_or_else(|| {
				format!(
					"Failed to get Ethereum block data for block {}",
					block_height
				)
			})?;

		// Get block with full transaction details and receipts using EthDataProvider
		let block_num = BlockNumberOrHash::Num(block_height.into());
		let rpc_block = self
			.eth_data_provider
			.block_by_number(block_num.clone(), true)
			.await
			.map_err(|e| format!("Failed to get block: {:?}", e))?
			.ok_or_else(|| format!("Block {} not found via EthApi", block_height))?;

		let rpc_receipts = self
			.eth_data_provider
			.block_transaction_receipts(block_num)
			.await
			.map_err(|e| format!("Failed to get receipts: {:?}", e))?
			.unwrap_or_default();

		// Extract transactions from RichBlock
		let rpc_transactions: Vec<Transaction> = match rpc_block.inner.transactions {
			BlockTransactions::Full(txs) => txs,
			BlockTransactions::Hashes(_) => {
				return Err("Expected full transactions but got hashes".to_string());
			}
		};

		let eth_block_hash = eth_block.header.hash();
		let process_start_timestamp = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.map(|d| d.as_secs())
			.unwrap_or(0);

		// Build DebankBlock
		// Note: ethereum::Header doesn't have base_fee_per_gas field, it can be obtained from
		// runtime API if needed. For now, we set it to None.
		let debank_block = DebankBlock {
			id: eth_block_hash,
			height: block_height as u64,
			parent_id: eth_block.header.parent_hash,
			base_fee_per_gas: None, // TODO: Can be obtained from Runtime API if needed
			miner: eth_block.header.beneficiary,
			gas_limit: eth_block.header.gas_limit.as_u64(),
			gas_used: eth_block.header.gas_used.as_u64(),
			timestamp: eth_block.header.timestamp,
			process_start_timestamp,
		};

		// Build DebankBlockHeader
		let debank_header = DebankBlockHeader {
			parent_hash: eth_block.header.parent_hash,
			sha3_uncles: eth_block.header.ommers_hash,
			miner: eth_block.header.beneficiary,
			state_root: eth_block.header.state_root,
			transactions_root: eth_block.header.transactions_root,
			receipts_root: eth_block.header.receipts_root,
			logs_bloom: eth_block.header.logs_bloom.0.to_vec(),
			difficulty: eth_block.header.difficulty,
			number: block_height as u64,
			gas_limit: eth_block.header.gas_limit.as_u64(),
			gas_used: eth_block.header.gas_used.as_u64(),
			timestamp: eth_block.header.timestamp,
			extra_data: eth_block.header.extra_data.clone(),
			mix_hash: eth_block.header.mix_hash,
			nonce: {
				let nonce_bytes = eth_block.header.nonce.0;
				u64::from_be_bytes(nonce_bytes)
			},
			base_fee_per_gas: None, // TODO: Can be obtained from Runtime API if needed
			hash: eth_block_hash,
		};

		// Build DebankTransactions using EthApi's Transaction type
		// This provides all fields already correctly computed (gas_price for EIP-1559, etc.)
		let mut debank_txs: Vec<DebankTransaction> = Vec::new();
		for (idx, rpc_tx) in rpc_transactions.iter().enumerate() {
			// Get gas_used and status from RPC receipt (already correctly calculated by EthApi)
			let (gas_used, tx_status) = rpc_receipts
				.get(idx)
				.map(|receipt| {
					let gas = receipt.gas_used.map(|g| g.as_u64()).unwrap_or(0);
					let status = receipt
						.status_code
						.map(|s| s.as_u64() == 1)
						.unwrap_or(false);
					(gas, status)
				})
				.unwrap_or((0, false));

			let debank_tx = DebankTransaction {
				id: rpc_tx.hash,
				from: rpc_tx.from,
				to: rpc_tx.to.unwrap_or_default(),
				gas_limit: rpc_tx.gas.as_u64(),
				gas_price: rpc_tx.gas_price.map(|p| p.as_u128()).unwrap_or(0),
				gas_used,
				status: tx_status,
				gas_fee_cap: rpc_tx
					.max_fee_per_gas
					.map(|f| f.as_u128())
					.unwrap_or_else(|| rpc_tx.gas_price.map(|p| p.as_u128()).unwrap_or(0)),
				gas_tip_cap: rpc_tx
					.max_priority_fee_per_gas
					.map(|f| f.as_u128())
					.unwrap_or(0),
				input: rpc_tx.input.0.clone(),
				nonce: rpc_tx.nonce.as_u64(),
				transaction_index: rpc_tx.transaction_index.map(|i| i.as_u64()).unwrap_or(idx as u64),
				value: rpc_tx.value,
			};
			debank_txs.push(debank_tx);
		}

		// For genesis block, return early with empty traces
		if block_height == 0 {
			let state_diff =
				debank::state_diff::empty_state_diff(eth_block.header.state_root, H256::zero());
			let block_file = BlockFile {
				block: debank_block,
				transactions: debank_txs,
				events: Vec::new(),
				traces: Vec::new(),
				error_events: Vec::new(),
				error_traces: Vec::new(),
				storage_contracts: Vec::new(),
			};
			let validation_hash = block_file.validation().validation_hash;
			return Ok(DebankOutput {
				block_file,
				header: debank_header,
				state_diff: debank::state_diff::encode_state_diff(&state_diff),
				validation_hash,
			});
		}

		// Get tx hashes for formatting (using rpc_transactions from EthApi)
		let tx_hashes: Vec<H256> = rpc_transactions.iter().map(|tx| tx.hash).collect();
		let parent_hash = eth_block.header.parent_hash;

		// Use the new Debank listener to trace the block directly
		let client = Arc::clone(&self.client);
		let backend = Arc::clone(&self.backend);
		let overrides = Arc::clone(&self.overrides);

		// Trace the block and format output (including account info queries) in blocking task
		let formatted = tokio::task::spawn_blocking(move || {
			Self::trace_debank_block_sync(
				client,
				backend,
				substrate_hash,
				overrides,
				eth_block_hash,
				parent_hash,
				tx_hashes,
			)
		})
		.await
		.map_err(|e| format!("Failed to spawn blocking task: {:?}", e))?
		.map_err(|e| format!("Failed to trace block: {}", e))?;

		// Convert formatter output to RPC output types
		let all_traces: Vec<RpcDebankTrace> = formatted
			.traces
			.into_iter()
			.map(|t| RpcDebankTrace {
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
				subtraces: t.subtraces,
				trace_address: t.trace_address,
				error: t.error,
			})
			.collect();

		let all_error_traces: Vec<RpcDebankTrace> = formatted
			.error_traces
			.into_iter()
			.map(|t| RpcDebankTrace {
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
				subtraces: t.subtraces,
				trace_address: t.trace_address,
				error: t.error,
			})
			.collect();

		let all_events: Vec<RpcDebankEvent> = formatted
			.events
			.into_iter()
			.map(|e| RpcDebankEvent {
				id: e.id,
				contract_id: e.contract_id,
				selector: e.selector,
				topics: e.topics,
				data: e.data,
				tx_id: e.tx_id,
				parent_trace_id: e.parent_trace_id,
				pos_in_parent_trace: e.pos_in_parent_trace,
				idx: e.idx,
			})
			.collect();

		let all_error_events: Vec<RpcDebankEvent> = formatted
			.error_events
			.into_iter()
			.map(|e| RpcDebankEvent {
				id: e.id,
				contract_id: e.contract_id,
				selector: e.selector,
				topics: e.topics,
				data: e.data,
				tx_id: e.tx_id,
				parent_trace_id: e.parent_trace_id,
				pos_in_parent_trace: e.pos_in_parent_trace,
				idx: e.idx,
			})
			.collect();

		// Encode state diff using RLP
		let state_diff_encoded = rlp::encode(&formatted.state_diff).to_vec();

		let block_file = BlockFile {
			block: debank_block,
			transactions: debank_txs,
			events: all_events,
			traces: all_traces,
			error_events: all_error_events,
			error_traces: all_error_traces,
			storage_contracts: formatted.storage_contracts,
		};

		let validation_hash = block_file.validation().validation_hash;

		Ok(DebankOutput {
			block_file,
			header: debank_header,
			state_diff: state_diff_encoded,
			validation_hash,
		})
	}

	/// Synchronous block tracing using the Debank listener.
	/// Returns the formatted Debank output including account info for state diff.
	fn trace_debank_block_sync(
		client: Arc<C>,
		backend: Arc<BE>,
		substrate_hash: H256,
		overrides: Arc<dyn StorageOverride<B>>,
		eth_block_hash: H256,
		parent_hash: H256,
		tx_hashes: Vec<H256>,
	) -> Result<moonbeam_client_evm_tracing::formatters::debank::DebankBlockOutput, String> {
		let api = client.runtime_api();
		let block_header = client
			.header(substrate_hash)
			.map_err(|e| format!("Error fetching block header: {:?}", e))?
			.ok_or_else(|| format!("Block {} not found", substrate_hash))?;

		let height = *block_header.number();
		let substrate_parent_hash = *block_header.parent_hash();

		// Get Ethereum block data
		let eth_transactions = overrides
			.current_transaction_statuses(substrate_hash)
			.ok_or_else(|| format!("Failed to get transaction statuses for block {}", height))?;

		let eth_tx_hashes: Vec<H256> = eth_transactions
			.iter()
			.map(|t| t.transaction_hash)
			.collect();

		// Get extrinsics
		let extrinsics = backend
			.blockchain()
			.body(substrate_hash)
			.map_err(|e| format!("Error fetching extrinsics: {:?}", e))?
			.ok_or_else(|| format!("Block {} extrinsics not found", height))?;

		// Get DebugRuntimeApi version
		let trace_api_version = api
			.api_version::<dyn DebugRuntimeApi<B>>(substrate_parent_hash)
			.map_err(|_| "Runtime api version call failed".to_string())?
			.ok_or_else(|| "DebugRuntimeApi not found".to_string())?;

		// Create and use the Debank listener
		let mut listener = DebankListener::new();

		let f = || -> Result<_, String> {
			let result = if trace_api_version >= 5 {
				api.trace_block(
					substrate_parent_hash,
					extrinsics,
					eth_tx_hashes,
					&block_header,
				)
			} else {
				// Get core runtime api version
				let core_api_version = api
					.api_version::<dyn sp_api::Core<B>>(substrate_parent_hash)
					.map_err(|_| "Runtime api version call failed (core)".to_string())?
					.ok_or_else(|| "Core API not found".to_string())?;

				// Initialize block
				if core_api_version >= 5 {
					api.initialize_block(substrate_parent_hash, &block_header)
						.map_err(|e| format!("Runtime api access error: {:?}", e))?;
				} else {
					#[allow(deprecated)]
					api.initialize_block_before_version_5(substrate_parent_hash, &block_header)
						.map_err(|e| format!("Runtime api access error: {:?}", e))?;
				}

				#[allow(deprecated)]
				api.trace_block_before_version_5(substrate_parent_hash, extrinsics, eth_tx_hashes)
			};

			result
				.map_err(|e| format!("Error replaying block {}: {:?}", height, e))?
				.map_err(|e| format!("Internal error replaying block {}: {:?}", height, e))?;

			Ok(())
		};

		listener.using(f)?;

		// Finish the last transaction
		listener.finish_transaction();

		// Create account_info_fn that queries the Runtime API for account state
		// Returns (NewAccount, code) tuple
		let account_info_fn = |address: H160| -> Option<(
			moonbeam_client_evm_tracing::types::block::NewAccount,
			Vec<u8>,
		)> {
			// Get account basic info (balance, nonce) from Runtime API
			let account = api.account_basic(substrate_hash, address).ok()?;

			// Get account code
			let code = overrides.account_code_at(substrate_hash, address).unwrap_or_default();

			// Calculate code hash using keccak256
			let code_hash = if code.is_empty() {
				H256::from_slice(&keccak_256(&[]))
			} else {
				H256::from_slice(&keccak_256(&code))
			};

			let new_account = moonbeam_client_evm_tracing::types::block::NewAccount {
				address,
				balance: account.balance,
				nonce: account.nonce.as_u64(),
				code_hash,
			};

			Some((new_account, code))
		};

		// Format the listener output to Debank format
		let formatted = DebankFormatter::format(
			listener,
			eth_block_hash,
			parent_hash,
			&tx_hashes,
			account_info_fn,
		);

		Ok(formatted)
	}
}

#[jsonrpsee::core::async_trait]
impl<B, C, BE> TraceServer for Trace<B, C, BE>
where
	BE: Backend<B> + 'static,
	BE::State: StateBackend<BlakeTwo256>,
	B: BlockT<Hash = H256> + Send + Sync + 'static,
	B::Header: HeaderT<Number = u32>,
	C: ProvideRuntimeApi<B>,
	C: StorageProvider<B, BE>,
	C: HeaderMetadata<B, Error = BlockChainError> + HeaderBackend<B>,
	C: Send + Sync + 'static,
	C::Api: BlockBuilder<B>,
	C::Api: DebugRuntimeApi<B>,
	C::Api: EthereumRuntimeRPCApi<B>,
	C::Api: ApiExt<B>,
{
	async fn filter(
		&self,
		filter: FilterRequest,
	) -> jsonrpsee::core::RpcResult<Vec<TransactionTrace>> {
		self.clone()
			.filter(filter)
			.await
			.map_err(fc_rpc::internal_err)
	}

	async fn debank_block(
		&self,
		block_id: RequestBlockId,
	) -> jsonrpsee::core::RpcResult<DebankOutput> {
		self.clone()
			.trace_debank_block(block_id)
			.await
			.map_err(fc_rpc::internal_err)
	}
}

/// Requests the cache task can accept.
enum CacheRequest {
	/// Fetch the traces for given block hash.
	/// The task will answer only when it has processed this block.
	GetTraces {
		/// Returns the array of traces or an error (Arc-wrapped for zero-copy sharing).
		sender: oneshot::Sender<SharedTxsTraceRes>,
		/// Hash of the block.
		block: H256,
	},
}

/// Allows to interact with the cache task.
#[derive(Clone)]
pub struct CacheRequester(mpsc::Sender<CacheRequest>);

impl CacheRequester {
	/// Fetch the traces for given block hash.
	/// If the block is already cached, returns immediately.
	/// If the block is being traced, waits for the result.
	/// If the block is not cached, triggers tracing and waits for the result.
	/// Returns Arc-wrapped traces for zero-copy sharing.
	#[instrument(skip(self))]
	pub async fn get_traces(&self, block: H256) -> SharedTxsTraceRes {
		let (response_tx, response_rx) = oneshot::channel();
		let sender = self.0.clone();

		sender
			.send(CacheRequest::GetTraces {
				sender: response_tx,
				block,
			})
			.await
			.map_err(|e| {
				Arc::new(format!(
					"Trace cache task is overloaded or closed. Error : {:?}",
					e
				))
			})?;

		response_rx
			.await
			.map_err(|e| {
				Arc::new(format!(
					"Trace cache task closed the response channel. Error : {:?}",
					e
				))
			})?
			.map_err(|arc_error| {
				Arc::new(format!("Failed to replay block. Error : {:?}", arc_error))
			})
	}
}

/// Entry in the wait list for a block being traced.
struct WaitListEntry {
	/// Time when this entry was created
	created_at: Instant,
	/// All requests waiting for this block to be traced
	waiters: Vec<oneshot::Sender<SharedTxsTraceRes>>,
}

/// Wait list for requests pending the same block trace.
/// Multiple concurrent requests for the same block will be added to this list
/// and all will receive the result once tracing completes.
type WaitList = HashMap<H256, WaitListEntry>;

/// Message sent from blocking trace tasks back to the main cache task.
enum BlockingTaskMessage {
	/// The tracing is finished and the result is sent to the main task.
	Finished {
		block_hash: H256,
		result: TxsTraceRes,
		duration: Duration,
	},
}

/// Prometheus metrics for trace filter cache operations.
struct CacheMetrics {
	/// Current size of the wait list (number of blocks being traced)
	wait_list_size: substrate_prometheus_endpoint::Gauge<substrate_prometheus_endpoint::U64>,
	/// Total requests that joined an existing wait list entry (deduplication)
	wait_list_joins_total:
		substrate_prometheus_endpoint::Counter<substrate_prometheus_endpoint::U64>,
	/// Total trace tasks spawned
	tasks_spawned_total: substrate_prometheus_endpoint::Counter<substrate_prometheus_endpoint::U64>,
	/// Total trace operations that timed out
	timeouts_total: substrate_prometheus_endpoint::Counter<substrate_prometheus_endpoint::U64>,
	/// Histogram of trace operation durations in seconds
	trace_duration_seconds: substrate_prometheus_endpoint::Histogram,
}

impl CacheMetrics {
	fn register(
		registry: &PrometheusRegistry,
	) -> Result<Self, substrate_prometheus_endpoint::PrometheusError> {
		Ok(Self {
			wait_list_size: substrate_prometheus_endpoint::register(
				substrate_prometheus_endpoint::Gauge::new(
					"trace_filter_wait_list_size",
					"Current number of blocks in the wait list being traced",
				)?,
				registry,
			)?,
			wait_list_joins_total: substrate_prometheus_endpoint::register(
				substrate_prometheus_endpoint::Counter::new(
					"trace_filter_wait_list_joins_total",
					"Total requests that joined an existing wait list entry",
				)?,
				registry,
			)?,
			tasks_spawned_total: substrate_prometheus_endpoint::register(
				substrate_prometheus_endpoint::Counter::new(
					"trace_filter_tasks_spawned_total",
					"Total trace tasks spawned",
				)?,
				registry,
			)?,
			timeouts_total: substrate_prometheus_endpoint::register(
				substrate_prometheus_endpoint::Counter::new(
					"trace_filter_timeouts_total",
					"Total trace operations that timed out",
				)?,
				registry,
			)?,
			trace_duration_seconds: substrate_prometheus_endpoint::register(
				substrate_prometheus_endpoint::Histogram::with_opts(
					substrate_prometheus_endpoint::HistogramOpts::new(
						"trace_filter_trace_duration_seconds",
						"Histogram of trace operation durations in seconds",
					)
					.buckets(vec![0.1, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0]),
				)?,
				registry,
			)?,
		})
	}
}

/// Type wrapper for the cache task, generic over the Client, Block and Backend types.
pub struct CacheTask<B, C, BE> {
	client: Arc<C>,
	backend: Arc<BE>,
	blocking_permits: Arc<Semaphore>,
	cache: LRUCacheByteLimited<H256, Arc<Vec<TransactionTrace>>>,
	wait_list: WaitList,
	metrics: Option<CacheMetrics>,
	_phantom: PhantomData<B>,
}

impl<B, C, BE> CacheTask<B, C, BE>
where
	BE: Backend<B> + 'static,
	BE::State: StateBackend<BlakeTwo256>,
	C: ProvideRuntimeApi<B>,
	C: StorageProvider<B, BE>,
	C: HeaderMetadata<B, Error = BlockChainError> + HeaderBackend<B>,
	C: Send + Sync + 'static,
	B: BlockT<Hash = H256> + Send + Sync + 'static,
	B::Header: HeaderT<Number = u32>,
	C::Api: BlockBuilder<B>,
	C::Api: DebugRuntimeApi<B>,
	C::Api: EthereumRuntimeRPCApi<B>,
	C::Api: ApiExt<B>,
{
	/// Create a new cache task.
	///
	/// Returns a Future that needs to be added to a tokio executor, and a handle allowing to
	/// send requests to the task.
	pub fn create(
		client: Arc<C>,
		backend: Arc<BE>,
		cache_size_bytes: u64,
		blocking_permits: Arc<Semaphore>,
		overrides: Arc<dyn StorageOverride<B>>,
		prometheus: Option<PrometheusRegistry>,
		spawn_handle: SpawnTaskHandle,
	) -> (impl Future<Output = ()>, CacheRequester) {
		// Communication with the outside world - bounded channel to prevent memory exhaustion
		let (requester_tx, mut requester_rx) = mpsc::channel(10_000);

		// Task running in the service.
		let task = async move {
			let (blocking_tx, mut blocking_rx) =
				mpsc::channel(blocking_permits.available_permits().saturating_mul(2));

			// Periodic cleanup interval for orphaned wait list entries
			let mut cleanup_interval = interval(Duration::from_secs(30));
			cleanup_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

			// Register metrics if prometheus registry is provided
			let metrics =
				prometheus
					.as_ref()
					.and_then(|registry| match CacheMetrics::register(registry) {
						Ok(metrics) => Some(metrics),
						Err(e) => {
							log::warn!(
								target: CACHE_LOG_TARGET,
								"Failed to register trace filter metrics: {:?}",
								e
							);
							None
						}
					});

			let mut inner = Self {
				client,
				backend,
				blocking_permits,
				cache: LRUCacheByteLimited::new(
					"trace-filter-blocks-cache",
					cache_size_bytes,
					prometheus,
				),
				wait_list: HashMap::new(),
				metrics,
				_phantom: Default::default(),
			};

			loop {
				select! {
					request = requester_rx.recv().fuse() => {
						match request {
							None => break,
							Some(CacheRequest::GetTraces {sender, block}) =>
								inner.request_get_traces(&blocking_tx, sender, block, overrides.clone(), &spawn_handle),
						}
					},
					message = blocking_rx.recv().fuse() => {
						if let Some(BlockingTaskMessage::Finished { block_hash, result, duration }) = message {
							inner.blocking_finished(block_hash, result, duration);
						}
					},
					_ = cleanup_interval.tick().fuse() => {
						inner.cleanup_wait_list();
					},
				}
			}
		}
		.instrument(tracing::debug_span!("trace_filter_cache"));

		(task, CacheRequester(requester_tx))
	}

	/// Handle a request to get traces for a specific block.
	/// - If cached: respond immediately
	/// - If pending: add to wait list
	/// - If new: spawn trace task and add to wait list
	fn request_get_traces(
		&mut self,
		blocking_tx: &mpsc::Sender<BlockingTaskMessage>,
		sender: oneshot::Sender<SharedTxsTraceRes>,
		block: H256,
		overrides: Arc<dyn StorageOverride<B>>,
		spawn_handle: &SpawnTaskHandle,
	) {
		log::trace!(
			target: CACHE_LOG_TARGET,
			"Request received: block={}, wait_list_size={}",
			block,
			self.wait_list.len()
		);

		// Check if block is already cached
		if let Some(cached) = self.cache.get(&block) {
			log::trace!(
				target: CACHE_LOG_TARGET,
				"Cache hit: block={}",
				block
			);
			// Cache hit - respond immediately with Arc::clone (cheap)
			let _ = sender.send(Ok(Arc::clone(&cached)));
			return;
		}

		// Check if block is currently being traced
		if let Some(entry) = self.wait_list.get_mut(&block) {
			log::trace!(
				target: CACHE_LOG_TARGET,
				"Joining wait list: block={}, waiters={}",
				block,
				entry.waiters.len()
			);
			entry.waiters.push(sender);

			// Increment deduplication metric
			if let Some(ref metrics) = self.metrics {
				metrics.wait_list_joins_total.inc();
			}

			return;
		}

		// Add sender to wait list for this new block
		self.wait_list.insert(
			block,
			WaitListEntry {
				created_at: Instant::now(),
				waiters: vec![sender],
			},
		);

		log::debug!(
			target: CACHE_LOG_TARGET,
			"Spawning trace task: block={}, available_permits={}",
			block,
			self.blocking_permits.available_permits()
		);

		// Update metrics
		if let Some(ref metrics) = self.metrics {
			metrics.tasks_spawned_total.inc();
			metrics.wait_list_size.set(self.wait_list.len() as u64);
		}

		// Spawn worker task to trace the block
		let blocking_permits = Arc::clone(&self.blocking_permits);
		let client = Arc::clone(&self.client);
		let backend = Arc::clone(&self.backend);
		let blocking_tx = blocking_tx.clone();
		let start_time = Instant::now();

		spawn_handle.spawn(
			"trace-block",
			Some("trace-filter"),
			async move {
				// Wait for permit to limit concurrent tracing operations
				let _permit = blocking_permits.acquire().await;

				// Perform block tracing in blocking task with timeout
				let result = match tokio::time::timeout(
					Duration::from_secs(TRACING_TIMEOUT_SECS),
					tokio::task::spawn_blocking(move || {
						Self::cache_block(client, backend, block, overrides)
					}),
				)
				.await
				{
					// Timeout occurred
					Err(_elapsed) => {
						log::error!(
							target: CACHE_LOG_TARGET,
							"Tracing timeout for block {}",
							block
						);
						Err(format!(
							"Tracing timeout after {} seconds",
							TRACING_TIMEOUT_SECS
						))
					}
					// Task completed
					Ok(join_result) => {
						match join_result {
							// Task panicked
							Err(join_err) => Err(format!("Tracing panicked: {:?}", join_err)),
							// Task succeeded, return its result
							Ok(trace_result) => trace_result,
						}
					}
				};

				// Send result back to main task
				let duration = start_time.elapsed();
				let _ = blocking_tx
					.send(BlockingTaskMessage::Finished {
						block_hash: block,
						result,
						duration,
					})
					.await;
			}
			.instrument(tracing::trace_span!("trace_block", block = %block)),
		);
	}

	/// Handle completion of a block trace task.
	/// Sends result to all waiting requests and caches it.
	/// Uses Arc for zero-copy sharing across multiple waiters.
	fn blocking_finished(&mut self, block_hash: H256, result: TxsTraceRes, duration: Duration) {
		// Get all waiting senders for this block
		if let Some(entry) = self.wait_list.remove(&block_hash) {
			let waiter_count = entry.waiters.len();

			// Update wait list size metric
			if let Some(ref metrics) = self.metrics {
				metrics.wait_list_size.set(self.wait_list.len() as u64);
				metrics
					.trace_duration_seconds
					.observe(duration.as_secs_f64());
			}

			match result {
				Ok(traces) => {
					let trace_count = traces.len();
					// Wrap successful result in Arc once
					let arc_traces = Arc::new(traces);

					log::debug!(
						target: CACHE_LOG_TARGET,
						"Trace completed: block={}, traces={}, waiters={}, cached=true, duration={:?}",
						block_hash,
						trace_count,
						waiter_count,
						duration
					);

					// Send Arc::clone to all waiters (cheap pointer copy, no data duplication)
					for sender in entry.waiters {
						let _ = sender.send(Ok(Arc::clone(&arc_traces)));
					}

					// Cache the Arc-wrapped result
					self.cache.put(block_hash, arc_traces);
				}
				Err(error) => {
					log::warn!(
						target: CACHE_LOG_TARGET,
						"Trace failed: block={}, waiters={}, error={}",
						block_hash,
						waiter_count,
						error
					);

					// Wrap error in Arc once
					let arc_error = Arc::new(error);

					// Send Arc::clone to all waiters (cheap pointer copy, no string duplication)
					for sender in entry.waiters {
						let _ = sender.send(Err(Arc::clone(&arc_error)));
					}
				}
			}
		}
	}

	/// Clean up orphaned wait list entries that have been pending too long.
	/// This handles cases where spawned tasks panic or get cancelled.
	fn cleanup_wait_list(&mut self) {
		let timeout = Duration::from_secs(TRACING_TIMEOUT_SECS + 10);
		let now = Instant::now();

		let mut to_remove = Vec::new();

		for (block_hash, entry) in &self.wait_list {
			if now.duration_since(entry.created_at) > timeout {
				log::warn!(
					target: CACHE_LOG_TARGET,
					"Cleaning up orphaned wait list entry for block {}",
					block_hash
				);
				to_remove.push(*block_hash);
			}
		}

		log::debug!(
			target: CACHE_LOG_TARGET,
			"Wait list status: active_blocks={}, timed_out_block_requests={}",
			self.wait_list.len(),
			to_remove.len()
		);

		// Increment timeout metric for each timed out block
		if !to_remove.is_empty() {
			if let Some(ref metrics) = self.metrics {
				for _ in &to_remove {
					metrics.timeouts_total.inc();
				}
			}
		}

		// Remove timed-out entries and notify waiters
		let timeout_error =
			Arc::new("Trace request timeout (task failed or was cancelled)".to_string());

		for block_hash in to_remove {
			if let Some(entry) = self.wait_list.remove(&block_hash) {
				for sender in entry.waiters {
					let _ = sender.send(Err(Arc::clone(&timeout_error)));
				}
			}
		}

		// Update wait list size metric after cleanup
		if let Some(ref metrics) = self.metrics {
			metrics.wait_list_size.set(self.wait_list.len() as u64);
		}
	}

	/// (In blocking task) Use the Runtime API to trace the block.
	#[instrument(skip(client, backend, overrides))]
	fn cache_block(
		client: Arc<C>,
		backend: Arc<BE>,
		substrate_hash: H256,
		overrides: Arc<dyn StorageOverride<B>>,
	) -> TxsTraceRes {
		// Get Substrate block data.
		let api = client.runtime_api();
		let block_header = client
			.header(substrate_hash)
			.map_err(|e| {
				format!(
					"Error when fetching substrate block {} header : {:?}",
					substrate_hash, e
				)
			})?
			.ok_or_else(|| format!("Substrate block {} don't exist", substrate_hash))?;

		let height = *block_header.number();
		let substrate_parent_hash = *block_header.parent_hash();

		// Get Ethereum block data.
		let (eth_block, eth_transactions) = match (
			overrides.current_block(substrate_hash),
			overrides.current_transaction_statuses(substrate_hash),
		) {
			(Some(a), Some(b)) => (a, b),
			_ => {
				return Err(format!(
					"Failed to get Ethereum block data for Substrate block {}",
					substrate_hash
				))
			}
		};

		let eth_block_hash = eth_block.header.hash();
		let eth_tx_hashes = eth_transactions
			.iter()
			.map(|t| t.transaction_hash)
			.collect();

		// Get extrinsics (containing Ethereum ones)
		let extrinsics = backend
			.blockchain()
			.body(substrate_hash)
			.map_err(|e| {
				format!(
					"Blockchain error when fetching extrinsics of block {} : {:?}",
					height, e
				)
			})?
			.ok_or_else(|| format!("Could not find block {} when fetching extrinsics.", height))?;

		// Get DebugRuntimeApi version
		let trace_api_version = if let Ok(Some(api_version)) =
			api.api_version::<dyn DebugRuntimeApi<B>>(substrate_parent_hash)
		{
			api_version
		} else {
			return Err("Runtime api version call failed (trace)".to_string());
		};

		// Trace the block.
		let f = || -> Result<_, String> {
			let result = if trace_api_version >= 5 {
				api.trace_block(
					substrate_parent_hash,
					extrinsics,
					eth_tx_hashes,
					&block_header,
				)
			} else {
				// Get core runtime api version
				let core_api_version = if let Ok(Some(api_version)) =
					api.api_version::<dyn Core<B>>(substrate_parent_hash)
				{
					api_version
				} else {
					return Err("Runtime api version call failed (core)".to_string());
				};

				// Initialize block: calls the "on_initialize" hook on every pallet
				// in AllPalletsWithSystem
				// This was fine before pallet-message-queue because the XCM messages
				// were processed by the "setValidationData" inherent call and not on an
				// "on_initialize" hook, which runs before enabling XCM tracing
				if core_api_version >= 5 {
					api.initialize_block(substrate_parent_hash, &block_header)
						.map_err(|e| format!("Runtime api access error: {:?}", e))?;
				} else {
					#[allow(deprecated)]
					api.initialize_block_before_version_5(substrate_parent_hash, &block_header)
						.map_err(|e| format!("Runtime api access error: {:?}", e))?;
				}

				#[allow(deprecated)]
				api.trace_block_before_version_5(substrate_parent_hash, extrinsics, eth_tx_hashes)
			};

			result
				.map_err(|e| format!("Blockchain error when replaying block {} : {:?}", height, e))?
				.map_err(|e| {
					tracing::warn!(
						target: "tracing",
						"Internal runtime error when replaying block {} : {:?}",
						height,
						e
					);
					format!(
						"Internal runtime error when replaying block {} : {:?}",
						height, e
					)
				})?;

			Ok(moonbeam_rpc_primitives_debug::Response::Block)
		};

		let eth_transactions_by_index: BTreeMap<u32, H256> = eth_transactions
			.iter()
			.map(|t| (t.transaction_index, t.transaction_hash))
			.collect();

		let mut proxy = moonbeam_client_evm_tracing::listeners::CallList::default();
		proxy.using(f)?;

		let traces: Vec<TransactionTrace> =
			moonbeam_client_evm_tracing::formatters::TraceFilter::format(proxy)
				.ok_or("Fail to format proxy")?
				.into_iter()
				.filter_map(|mut trace| {
					match eth_transactions_by_index.get(&trace.transaction_position) {
						Some(transaction_hash) => {
							trace.block_hash = eth_block_hash;
							trace.block_number = height;
							trace.transaction_hash = *transaction_hash;

							// Reformat error messages.
							if let block::TransactionTraceOutput::Error(ref mut error) =
								trace.output
							{
								if error.as_slice() == b"execution reverted" {
									*error = b"Reverted".to_vec();
								}
							}

							Some(trace)
						}
						None => {
							log::warn!(
								target: "tracing",
								"A trace in block {} does not map to any known ethereum transaction. Trace: {:?}",
								height,
								trace,
							);
							None
						}
					}
				})
				.collect();

		Ok(traces)
	}
}

// Implement EthDataProvider for Eth (EthApi)
#[async_trait::async_trait]
impl<B, C, P, CT, BE, CIDP, EC> EthDataProvider for Eth<B, C, P, CT, BE, CIDP, EC>
where
	B: BlockT,
	C: ProvideRuntimeApi<B>,
	C::Api: EthereumRuntimeRPCApi<B>,
	C: HeaderBackend<B> + StorageProvider<B, BE> + 'static,
	BE: Backend<B> + 'static,
	P: sc_service::TransactionPool<Block = B, Hash = B::Hash> + 'static,
	CT: Send + Sync + 'static,
	CIDP: Send + Sync + 'static,
	EC: EthConfig<B, C>,
{
	async fn block_transaction_receipts(
		&self,
		number_or_hash: BlockNumberOrHash,
	) -> RpcResult<Option<Vec<Receipt>>> {
		Eth::block_transaction_receipts(self, number_or_hash).await
	}

	async fn block_by_number(
		&self,
		number_or_hash: BlockNumberOrHash,
		full: bool,
	) -> RpcResult<Option<RichBlock>> {
		Eth::block_by_number(self, number_or_hash, full).await
	}
}
