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

//! Types for tracing all Ethereum transactions of a block.

use super::serialization::*;
use rlp::{Encodable, RlpStream};
use serde::Serialize;

use ethereum_types::{H160, H256, U256};
use parity_scale_codec::{Decode, Encode};
use sp_std::vec::Vec;

/// Block transaction trace.
#[derive(Clone, Eq, PartialEq, Debug, Encode, Decode, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockTransactionTrace {
	#[serde(serialize_with = "h256_0x_serialize")]
	pub tx_hash: H256,
	pub result: crate::types::single::TransactionTrace,
	#[serde(skip_serializing)]
	pub tx_position: u32,
}

#[derive(Clone, Eq, PartialEq, Debug, Encode, Decode, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TransactionTrace {
	#[serde(flatten)]
	pub action: TransactionTraceAction,
	#[serde(serialize_with = "h256_0x_serialize")]
	pub block_hash: H256,
	pub block_number: u32,
	#[serde(flatten)]
	pub output: TransactionTraceOutput,
	pub subtraces: u32,
	pub trace_address: Vec<u32>,
	#[serde(serialize_with = "h256_0x_serialize")]
	pub transaction_hash: H256,
	pub transaction_position: u32,
}

#[derive(Clone, Eq, PartialEq, Debug, Encode, Decode, Serialize)]
#[serde(rename_all = "camelCase", tag = "type", content = "action")]
pub enum TransactionTraceAction {
	#[serde(rename_all = "camelCase")]
	Call {
		call_type: super::CallType,
		from: H160,
		gas: U256,
		#[serde(serialize_with = "bytes_0x_serialize")]
		input: Vec<u8>,
		to: H160,
		value: U256,
	},
	#[serde(rename_all = "camelCase")]
	Create {
		creation_method: super::CreateType,
		from: H160,
		gas: U256,
		#[serde(serialize_with = "bytes_0x_serialize")]
		init: Vec<u8>,
		value: U256,
	},
	#[serde(rename_all = "camelCase")]
	Suicide {
		address: H160,
		balance: U256,
		refund_address: H160,
	},
}

#[derive(Clone, Eq, PartialEq, Debug, Encode, Decode, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TransactionTraceOutput {
	Result(TransactionTraceResult),
	Error(#[serde(serialize_with = "string_serialize")] Vec<u8>),
}

#[derive(Clone, Eq, PartialEq, Debug, Encode, Decode, Serialize)]
#[serde(rename_all = "camelCase", untagged)]
pub enum TransactionTraceResult {
	#[serde(rename_all = "camelCase")]
	Call {
		gas_used: U256,
		#[serde(serialize_with = "bytes_0x_serialize")]
		output: Vec<u8>,
	},
	#[serde(rename_all = "camelCase")]
	Create {
		address: H160,
		#[serde(serialize_with = "bytes_0x_serialize")]
		code: Vec<u8>,
		gas_used: U256,
	},
	Suicide,
}

/// New account created in a block
#[derive(Clone, Eq, PartialEq, Debug, Encode, Decode, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewAccount {
	#[serde(serialize_with = "h256_0x_serialize")]
	pub address: H256,
	#[serde(serialize_with = "u256_0x_serialize")]
	pub balance: U256,
	pub nonce: u64,
	#[serde(serialize_with = "h256_0x_serialize")]
	pub code_hash: H256,
}

/// New contract code deployed in a block
#[derive(Clone, Eq, PartialEq, Debug, Encode, Decode, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NewCode {
	#[serde(serialize_with = "h256_0x_serialize")]
	pub code_hash: H256,
	#[serde(serialize_with = "bytes_0x_serialize")]
	pub code: Vec<u8>,
}

/// Index-value pair for storage diff
#[derive(Clone, Eq, PartialEq, Debug, Encode, Decode, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexValuePair {
	#[serde(serialize_with = "h256_0x_serialize")]
	pub index: H256,
	#[serde(serialize_with = "u256_0x_serialize")]
	pub value: U256,
}

/// Storage diff for a single account
#[derive(Clone, Eq, PartialEq, Debug, Encode, Decode, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountStorageDiff {
	#[serde(serialize_with = "h256_0x_serialize")]
	pub address: H256,
	pub values: Vec<IndexValuePair>,
}

/// Block storage diff for trace_blockDiff/trace_debankBlock RPC
#[derive(Clone, Eq, PartialEq, Debug, Encode, Decode, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockStorageDiff {
	#[serde(serialize_with = "h256_0x_serialize")]
	pub hash: H256,
	#[serde(serialize_with = "h256_0x_serialize")]
	pub parent_hash: H256,
	pub new_accounts: Vec<NewAccount>,
	pub deleted_accounts: Vec<H256>,
	pub storage_diff: Vec<AccountStorageDiff>,
	pub new_codes: Vec<NewCode>,
}

/// Convert H160 address to H256 via keccak256 hash.
/// Go consumer uses common.Hash (32 bytes) for address fields.
pub fn address_to_hash(address: &H160) -> H256 {
	use sha3::{Digest, Keccak256};
	H256::from_slice(&Keccak256::digest(address.as_bytes()))
}

// RLP encoding implementations for state diff types

impl Encodable for NewAccount {
	fn rlp_append(&self, s: &mut RlpStream) {
		s.begin_list(4);
		s.append(&self.address);
		s.append(&self.balance);
		s.append(&self.nonce);
		s.append(&self.code_hash);
	}
}

impl Encodable for NewCode {
	fn rlp_append(&self, s: &mut RlpStream) {
		s.begin_list(2);
		s.append(&self.code_hash);
		s.append(&self.code);
	}
}

impl Encodable for IndexValuePair {
	fn rlp_append(&self, s: &mut RlpStream) {
		s.begin_list(2);
		s.append(&self.index);
		s.append(&self.value);
	}
}

impl Encodable for AccountStorageDiff {
	fn rlp_append(&self, s: &mut RlpStream) {
		s.begin_list(2);
		s.append(&self.address);
		s.append_list(&self.values);
	}
}

impl Encodable for BlockStorageDiff {
	fn rlp_append(&self, s: &mut RlpStream) {
		s.begin_list(6);
		s.append(&self.hash);
		s.append(&self.parent_hash);
		s.append_list(&self.new_accounts);
		s.append_list(&self.deleted_accounts);
		s.append_list(&self.storage_diff);
		s.append_list(&self.new_codes);
	}
}

#[cfg(test)]
mod rlp_compat_tests {
	use super::*;
	use alloy_primitives::{Bytes, B256, U256 as AlloyU256};
	use alloy_rlp::Decodable;

	/// Mirror types using alloy-rlp derive macros.
	/// These match the Go consumer's types where addresses are common.Hash (32 bytes).

	#[derive(alloy_rlp::RlpDecodable, Debug, PartialEq)]
	struct AlloyNewAccount {
		address: B256,
		balance: AlloyU256,
		nonce: u64,
		code_hash: B256,
	}

	#[derive(alloy_rlp::RlpDecodable, Debug, PartialEq)]
	struct AlloyNewCode {
		code_hash: B256,
		code: Bytes,
	}

	#[derive(alloy_rlp::RlpDecodable, Debug, PartialEq)]
	struct AlloyIndexValuePair {
		index: B256,
		value: AlloyU256,
	}

	#[derive(alloy_rlp::RlpDecodable, Debug, PartialEq)]
	struct AlloyAccountStorageDiff {
		address: B256,
		values: Vec<AlloyIndexValuePair>,
	}

	#[derive(alloy_rlp::RlpDecodable, Debug, PartialEq)]
	struct AlloyBlockStorageDiff {
		hash: B256,
		parent_hash: B256,
		new_accounts: Vec<AlloyNewAccount>,
		deleted_accounts: Vec<B256>,
		storage_diff: Vec<AlloyAccountStorageDiff>,
		new_codes: Vec<AlloyNewCode>,
	}

	#[test]
	fn rlp_alloy_compat_empty_diff() {
		let diff = BlockStorageDiff {
			hash: H256::from_low_u64_be(1),
			parent_hash: H256::from_low_u64_be(2),
			new_accounts: vec![],
			deleted_accounts: vec![],
			storage_diff: vec![],
			new_codes: vec![],
		};

		let encoded = rlp::encode(&diff);
		let decoded = AlloyBlockStorageDiff::decode(&mut encoded.as_ref())
			.expect("alloy-rlp should decode empty BlockStorageDiff");

		assert_eq!(decoded.hash, B256::from(diff.hash.0));
		assert_eq!(decoded.parent_hash, B256::from(diff.parent_hash.0));
		assert!(decoded.new_accounts.is_empty());
		assert!(decoded.deleted_accounts.is_empty());
		assert!(decoded.storage_diff.is_empty());
		assert!(decoded.new_codes.is_empty());
	}

	#[test]
	fn rlp_alloy_compat_full_diff() {
		let addr_hash_1 = H256::from_low_u64_be(0x1234);
		let addr_hash_2 = H256::from_low_u64_be(0x5678);

		let diff = BlockStorageDiff {
			hash: H256::from_low_u64_be(0xaabb),
			parent_hash: H256::from_low_u64_be(0xccdd),
			new_accounts: vec![
				NewAccount {
					address: addr_hash_1,
					balance: U256::from(999_999),
					nonce: 42,
					code_hash: H256::from_low_u64_be(0xdead),
				},
				NewAccount {
					address: addr_hash_2,
					balance: U256::zero(),
					nonce: 0,
					code_hash: H256::zero(),
				},
			],
			deleted_accounts: vec![
				H256::from_low_u64_be(0xaaaa),
				H256::from_low_u64_be(0xbbbb),
			],
			storage_diff: vec![
				AccountStorageDiff {
					address: H256::from_low_u64_be(0x1111),
					values: vec![
						IndexValuePair {
							index: H256::from_low_u64_be(0),
							value: U256::from(100),
						},
						IndexValuePair {
							index: H256::from_low_u64_be(1),
							value: U256::from(200),
						},
					],
				},
				AccountStorageDiff {
					address: H256::from_low_u64_be(0x2222),
					values: vec![],
				},
			],
			new_codes: vec![NewCode {
				code_hash: H256::from_low_u64_be(0xc0de),
				code: vec![0x60, 0x80, 0x60, 0x40, 0x52, 0x34, 0x80, 0x15],
			}],
		};

		let encoded = rlp::encode(&diff);
		let decoded = AlloyBlockStorageDiff::decode(&mut encoded.as_ref())
			.expect("alloy-rlp should decode full BlockStorageDiff");

		assert_eq!(decoded.hash, B256::from(diff.hash.0));
		assert_eq!(decoded.parent_hash, B256::from(diff.parent_hash.0));

		assert_eq!(decoded.new_accounts.len(), 2);
		assert_eq!(decoded.new_accounts[0].address, B256::from(addr_hash_1.0));
		assert_eq!(decoded.new_accounts[0].balance, AlloyU256::from(999_999u64));
		assert_eq!(decoded.new_accounts[0].nonce, 42);
		assert_eq!(decoded.new_accounts[1].nonce, 0);
		assert_eq!(decoded.new_accounts[1].balance, AlloyU256::ZERO);

		assert_eq!(decoded.deleted_accounts.len(), 2);
		assert_eq!(
			decoded.deleted_accounts[0],
			B256::from(H256::from_low_u64_be(0xaaaa).0)
		);

		assert_eq!(decoded.storage_diff.len(), 2);
		assert_eq!(
			decoded.storage_diff[0].address,
			B256::from(H256::from_low_u64_be(0x1111).0)
		);
		assert_eq!(decoded.storage_diff[0].values.len(), 2);
		assert_eq!(decoded.storage_diff[0].values[0].value, AlloyU256::from(100u64));
		assert_eq!(decoded.storage_diff[1].values.len(), 0);

		assert_eq!(decoded.new_codes.len(), 1);
		assert_eq!(
			decoded.new_codes[0].code.as_ref(),
			&[0x60, 0x80, 0x60, 0x40, 0x52, 0x34, 0x80, 0x15]
		);
	}

	#[test]
	fn rlp_alloy_compat_large_values() {
		let diff = BlockStorageDiff {
			hash: H256::repeat_byte(0xff),
			parent_hash: H256::repeat_byte(0xaa),
			new_accounts: vec![NewAccount {
				address: H256::repeat_byte(0xff),
				balance: U256::MAX,
				nonce: u64::MAX,
				code_hash: H256::repeat_byte(0xff),
			}],
			deleted_accounts: vec![],
			storage_diff: vec![],
			new_codes: vec![NewCode {
				code_hash: H256::repeat_byte(0xbb),
				code: vec![0xfe; 1024],
			}],
		};

		let encoded = rlp::encode(&diff);
		let decoded = AlloyBlockStorageDiff::decode(&mut encoded.as_ref())
			.expect("alloy-rlp should decode BlockStorageDiff with large values");

		assert_eq!(decoded.new_accounts[0].nonce, u64::MAX);
		assert_eq!(decoded.new_accounts[0].balance, AlloyU256::MAX);
		assert_eq!(decoded.new_codes[0].code.len(), 1024);
	}
}
