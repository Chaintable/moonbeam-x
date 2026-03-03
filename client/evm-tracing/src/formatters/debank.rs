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

//! Debank trace formatter - converts listener data to Debank output format.

use crate::listeners::debank::{CallFrame, DebankCallType, Listener};
use crate::types::block::{
	AccountStorageDiff, BlockStorageDiff, IndexValuePair, NewAccount, NewCode,
};
use ethereum_types::{H160, H256, U256};
use std::collections::HashMap;

/// Debank trace output for a single call
#[derive(Debug, Clone)]
pub struct DebankTrace {
	pub id: String,
	pub from_addr: H160,
	pub gas_limit: u64,
	pub input: Vec<u8>,
	pub to_addr: H160,
	pub value: U256,
	pub gas_used: u64,
	pub output: Vec<u8>,
	pub call_create_type: String,
	pub call_type: String,
	pub tx_id: H256,
	pub parent_trace_id: String,
	pub pos_in_parent_trace: usize,
	pub self_storage_change: bool,
	pub storage_change: bool,
	pub subtraces: usize,
	pub trace_address: Vec<usize>,
	pub error: String,
}

/// Debank event output
#[derive(Debug, Clone)]
pub struct DebankEvent {
	pub id: String,
	pub contract_id: H160,
	pub selector: String,
	pub topics: Vec<String>,
	pub data: Vec<u8>,
	pub tx_id: H256,
	pub parent_trace_id: String,
	pub pos_in_parent_trace: usize,
	pub idx: usize,
}

/// Complete Debank output for a block
#[derive(Debug, Clone)]
pub struct DebankBlockOutput {
	pub traces: Vec<DebankTrace>,
	pub error_traces: Vec<DebankTrace>,
	pub events: Vec<DebankEvent>,
	pub error_events: Vec<DebankEvent>,
	pub storage_contracts: Vec<H160>,
	pub state_diff: BlockStorageDiff,
}

pub struct Formatter;

impl Formatter {
	/// Format listener data into Debank output.
	/// tx_hashes: mapping from transaction index to transaction hash
	/// account_info_fn: callback to get account info for state diff
	///   Returns (NewAccount, code) where code is the contract bytecode (empty for EOAs)
	pub fn format<F>(
		mut listener: Listener,
		block_hash: H256,
		parent_hash: H256,
		tx_hashes: &[H256],
		account_info_fn: F,
	) -> DebankBlockOutput
	where
		F: Fn(H160) -> Option<(NewAccount, Vec<u8>)>,
	{
		// Finish the last transaction if not already done
		listener.finish_transaction();

		let mut traces = Vec::new();
		let mut error_traces = Vec::new();
		let mut events = Vec::new();
		let mut error_events = Vec::new();

		// Process each transaction's call frame
		for (tx_idx, frame) in listener.completed_frames.iter().enumerate() {
			let tx_hash = tx_hashes.get(tx_idx).copied().unwrap_or_default();

			// Generate root trace ID
			let root_trace_id = calculate_debank_id(&[&format!("{tx_hash:?}"), "", "0"]);

			// Convert root frame to trace
			let root_trace = frame_to_trace(frame, tx_hash, String::new(), &root_trace_id);
			if frame.failed {
				error_traces.push(root_trace);
			} else {
				traces.push(root_trace);
			}

			// Recursively process children and events
			process_frame_children(
				frame,
				tx_hash,
				&root_trace_id,
				&[],
				&mut traces,
				&mut error_traces,
				&mut events,
				&mut error_events,
			);
		}

		// Build state diff
		let state_diff = format_state_diff(&listener, block_hash, parent_hash, account_info_fn);

		// Collect storage contracts
		let storage_contracts: Vec<H160> = listener.storage_contracts.into_iter().collect();

		DebankBlockOutput {
			traces,
			error_traces,
			events,
			error_events,
			storage_contracts,
			state_diff,
		}
	}
}

fn frame_to_trace(
	frame: &CallFrame,
	tx_hash: H256,
	parent_trace_id: String,
	trace_id: &str,
) -> DebankTrace {
	let (call_create_type, call_type) = match frame.call_type {
		DebankCallType::Call => ("call".to_string(), "call".to_string()),
		DebankCallType::CallCode => ("call".to_string(), "callcode".to_string()),
		DebankCallType::DelegateCall => ("call".to_string(), "delegatecall".to_string()),
		DebankCallType::StaticCall => ("call".to_string(), "staticcall".to_string()),
		DebankCallType::Create => ("create".to_string(), String::new()),
		DebankCallType::Suicide => ("suicide".to_string(), String::new()),
	};

	DebankTrace {
		id: trace_id.to_string(),
		from_addr: frame.from,
		gas_limit: frame.gas,
		input: frame.input.clone(),
		to_addr: frame.to,
		value: frame.value,
		gas_used: frame.gas_used,
		output: frame.output.clone(),
		call_create_type,
		call_type,
		tx_id: tx_hash,
		parent_trace_id,
		pos_in_parent_trace: frame.pos_in_parent_trace,
		self_storage_change: frame.self_storage_change,
		storage_change: frame.storage_change,
		subtraces: frame.calls.len(),
		trace_address: frame.trace_address.clone(),
		error: frame.error.clone(),
	}
}

fn process_frame_children(
	frame: &CallFrame,
	tx_hash: H256,
	parent_trace_id: &str,
	parent_trace_address: &[usize],
	traces: &mut Vec<DebankTrace>,
	error_traces: &mut Vec<DebankTrace>,
	events: &mut Vec<DebankEvent>,
	error_events: &mut Vec<DebankEvent>,
) {
	// Process child calls
	for (i, child) in frame.calls.iter().enumerate() {
		let child_trace_id = calculate_debank_id(&[
			&format!("{tx_hash:?}"),
			parent_trace_id,
			&child.pos_in_parent_trace.to_string(),
		]);

		let mut trace_address = parent_trace_address.to_vec();
		trace_address.push(i);

		let trace = frame_to_trace(child, tx_hash, parent_trace_id.to_string(), &child_trace_id);

		if child.failed {
			error_traces.push(trace);
		} else {
			traces.push(trace);
		}

		// Recursively process grandchildren
		process_frame_children(
			child,
			tx_hash,
			&child_trace_id,
			&trace_address,
			traces,
			error_traces,
			events,
			error_events,
		);
	}

	// Process logs/events
	for log in &frame.logs {
		let event_id = calculate_debank_id(&[parent_trace_id, &log.position.to_string()]);

		let selector = log
			.topics
			.first()
			.map(|t| format!("{t:?}"))
			.unwrap_or_default();
		let topics: Vec<String> = if log.topics.len() > 1 {
			log.topics[1..].iter().map(|t| format!("{t:?}")).collect()
		} else {
			Vec::new()
		};

		let event = DebankEvent {
			id: event_id,
			contract_id: log.address,
			selector,
			topics,
			data: log.data.clone(),
			tx_id: tx_hash,
			parent_trace_id: parent_trace_id.to_string(),
			pos_in_parent_trace: log.position,
			idx: log.log_index,
		};

		if frame.failed || frame.parent_failed {
			error_events.push(event);
		} else {
			events.push(event);
		}
	}
}

fn format_state_diff<F>(
	listener: &Listener,
	block_hash: H256,
	parent_hash: H256,
	account_info_fn: F,
) -> BlockStorageDiff
where
	F: Fn(H160) -> Option<(NewAccount, Vec<u8>)>,
{
	let mut new_accounts = Vec::new();
	let mut new_codes_map: HashMap<H256, Vec<u8>> = HashMap::new();
	let mut storage_diff = Vec::new();

	// Process touched accounts
	log::debug!(
		target: "tracing",
		"format_state_diff: touched_accounts count = {}, addresses = {:?}",
		listener.touched_accounts.len(),
		listener.touched_accounts
	);
	for address in &listener.touched_accounts {
		if listener.deleted_accounts.contains(address) {
			continue;
		}

		if let Some((account, code)) = account_info_fn(*address) {
			// Track new code for created contracts
			if listener.created_accounts.contains(address) && !code.is_empty() {
				new_codes_map.insert(account.code_hash, code);
			}
			new_accounts.push(account);
		} else {
			log::warn!(
				target: "tracing",
				"account_info_fn returned None for touched account {:?}",
				address
			);
		}
	}

	// Process storage changes
	let mut storage_addresses: Vec<_> = listener.storage_changes.keys().cloned().collect();
	storage_addresses.sort();

	for address in storage_addresses {
		if let Some(changes) = listener.storage_changes.get(&address) {
			let mut values: Vec<IndexValuePair> = changes
				.iter()
				.map(|(index, value)| IndexValuePair {
					index: *index,
					value: U256::from_big_endian(value.as_bytes()),
				})
				.collect();

			values.sort_by_key(|p| p.index);

			if !values.is_empty() {
				storage_diff.push(AccountStorageDiff { address, values });
			}
		}
	}

	// Sort for deterministic output
	new_accounts.sort_by_key(|a| a.address);

	let mut new_codes: Vec<NewCode> = new_codes_map
		.into_iter()
		.map(|(code_hash, code)| NewCode { code_hash, code })
		.collect();
	new_codes.sort_by_key(|c| c.code_hash);

	let mut deleted_accounts: Vec<H160> = listener.deleted_accounts.iter().cloned().collect();
	deleted_accounts.sort();

	BlockStorageDiff {
		hash: block_hash,
		parent_hash,
		new_accounts,
		deleted_accounts,
		storage_diff,
		new_codes,
	}
}

/// Calculate Debank ID from components using MD5 hash.
pub fn calculate_debank_id(args: &[&str]) -> String {
	use md5::{Digest, Md5};

	let mut hasher = Md5::new();
	for arg in args {
		hasher.update(arg.as_bytes());
	}
	hex::encode(hasher.finalize())
}
