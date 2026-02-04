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

//! Debank data types for trace_debankBlock RPC output.

use ethereum_types::{H160, H256, U256};
use serde::{Deserialize, Serialize};

/// Debank block information.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[serde(default)]
pub struct DebankBlock {
	pub id: H256,
	pub height: u64,
	pub parent_id: H256,
	pub base_fee_per_gas: Option<u64>,
	pub miner: H160,
	pub gas_limit: u64,
	pub gas_used: u64,
	pub timestamp: u64,
	pub process_start_timestamp: u64,
}

/// Debank transaction information.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[serde(default)]
pub struct DebankTransaction {
	pub id: H256,
	#[serde(rename = "from_addr")]
	pub from: H160,
	#[serde(rename = "to_addr")]
	pub to: H160,
	pub gas_limit: u64,
	pub gas_price: u128,
	pub gas_used: u64,
	pub status: bool,
	#[serde(rename = "max_fee_per_gas")]
	pub gas_fee_cap: u128,
	#[serde(rename = "max_priority_fee_per_gas")]
	pub gas_tip_cap: u128,
	#[serde(serialize_with = "serialize_bytes")]
	pub input: Vec<u8>,
	pub nonce: u64,
	#[serde(rename = "idx")]
	pub transaction_index: u64,
	pub value: U256,
}

/// Debank event (log) information.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
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
	pub idx: usize,
}

/// Debank trace information.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(default)]
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
	pub subtraces: usize,
	pub trace_address: Vec<usize>,
	#[serde(skip_serializing_if = "String::is_empty")]
	pub error: String,
}

/// Block validation information.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct BlockValidation {
	pub validation_hash: i64,
	pub is_fork: bool,
}

/// Block file containing all debank data for a block.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[serde(default)]
pub struct BlockFile {
	pub block: DebankBlock,
	#[serde(rename = "txs")]
	pub transactions: Vec<DebankTransaction>,
	pub events: Vec<DebankEvent>,
	pub traces: Vec<DebankTrace>,
	pub error_events: Vec<DebankEvent>,
	pub error_traces: Vec<DebankTrace>,
	pub storage_contracts: Vec<H160>,
}

impl BlockFile {
	/// Calculate the validation hash for this block file.
	pub fn validation(&self) -> BlockValidation {
		let mut ids = Vec::new();
		ids.push(format!("{:?}", self.block.id));
		for transaction in self.transactions.iter() {
			ids.push(format!("{:?}", transaction.id));
		}
		for event in self.events.iter() {
			ids.push(event.id.clone());
		}
		for trace in self.traces.iter() {
			ids.push(trace.id.clone());
		}
		BlockValidation {
			validation_hash: calc_validation_hash(&ids),
			is_fork: false,
		}
	}
}

/// Block header in Debank format.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DebankBlockHeader {
	pub parent_hash: H256,
	pub sha3_uncles: H256,
	pub miner: H160,
	pub state_root: H256,
	pub transactions_root: H256,
	pub receipts_root: H256,
	#[serde(serialize_with = "serialize_bytes")]
	pub logs_bloom: Vec<u8>,
	pub difficulty: U256,
	pub number: u64,
	pub gas_limit: u64,
	pub gas_used: u64,
	pub timestamp: u64,
	#[serde(serialize_with = "serialize_bytes")]
	pub extra_data: Vec<u8>,
	pub mix_hash: H256,
	pub nonce: u64,
	pub base_fee_per_gas: Option<u64>,
	pub hash: H256,
}

/// The output of trace_debankBlock RPC method.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct DebankOutput {
	pub block_file: BlockFile,
	pub header: DebankBlockHeader,
	#[serde(serialize_with = "serialize_bytes")]
	pub state_diff: Vec<u8>,
	pub validation_hash: i64,
}

/// Calculate validation hash from a list of IDs.
pub fn calc_validation_hash(ids: &[String]) -> i64 {
	// Simple hash calculation for validation
	// Uses a basic additive hash over the string bytes
	let mut sum: u64 = 0;
	for id in ids {
		for byte in id.bytes() {
			sum = sum.wrapping_add(byte as u64);
		}
	}
	// Take last 6 digits
	(sum % 1_000_000) as i64
}

/// Calculate debank ID using MD5 hash.
pub fn calculate_debank_id(args: &[&str]) -> String {
	use md5::{Digest, Md5};

	let mut hasher = Md5::new();
	for arg in args {
		hasher.update(arg.as_bytes());
	}
	hex::encode(hasher.finalize())
}

/// Serialize bytes as hex string with 0x prefix.
fn serialize_bytes<S>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error>
where
	S: serde::Serializer,
{
	let hex_string = format!("0x{}", hex::encode(bytes));
	serializer.serialize_str(&hex_string)
}
