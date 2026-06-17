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

//! State diff computation for Debank trace output.

use super::types::BlockStorageDiff;
use ethereum_types::{H160, H256};
use moonbeam_client_evm_tracing::types::block::OverlayStateDelta;
use parity_scale_codec::Decode;
use sp_core::twox_128;

/// Create an empty state diff (for blocks with no state changes or genesis).
pub fn empty_state_diff(state_root: H256, parent_state_root: H256) -> BlockStorageDiff {
	BlockStorageDiff {
		hash: state_root,
		parent_hash: parent_state_root,
		new_accounts: Vec::new(),
		deleted_accounts: Vec::new(),
		storage_diff: Vec::new(),
		new_codes: Vec::new(),
	}
}

/// Encode a BlockStorageDiff using RLP for inclusion in DebankOutput.
pub fn encode_state_diff(state_diff: &BlockStorageDiff) -> Vec<u8> {
	rlp::encode(state_diff).to_vec()
}

// Layout constants for storage key parsing.
// All three maps use Blake2_128Concat, which produces `blake2_128(key) || key`
// (16 bytes of hash followed by the raw key).
//
//   System::Account:      twox_128("System") ++ twox_128("Account")
//                       ++ blake2_128(H160) ++ H160                          = 32 + 16 + 20 = 68
//   EVM::AccountCodes:    twox_128("EVM")    ++ twox_128("AccountCodes")
//                       ++ blake2_128(H160) ++ H160                          = 32 + 16 + 20 = 68
//   EVM::AccountStorages: twox_128("EVM")    ++ twox_128("AccountStorages")
//                       ++ blake2_128(H160) ++ H160
//                       ++ blake2_128(H256) ++ H256                          = 32 + 16 + 20 + 16 + 32 = 116
const PREFIX_LEN: usize = 32;
const SYS_KEY_LEN: usize = 68;
const CODES_KEY_LEN: usize = 68;
const STORAGES_KEY_LEN: usize = 116;
const ADDR_OFFSET: usize = PREFIX_LEN + 16; // 48
const SLOT_OFFSET: usize = ADDR_OFFSET + 20 + 16; // 84

/// Scan the post-execution storage overlay and derive an `OverlayStateDelta`.
///
/// This is the *authoritative* source of state diff: the overlay represents the
/// net change parent-state → current-state, after all revert-handling has been
/// applied by the underlying `OverlayedChanges`. It therefore does NOT suffer
/// from the SSTORE-in-reverted-frame ghost-diff problem of event-stream tracing.
///
/// Three storage maps are inspected:
///   - `System::Account`      — authoritative account birth / death signal.
///   - `EVM::AccountCodes`    — code deploy / clear (includes EIP-7702 reset).
///   - `EVM::AccountStorages` — EVM storage slot net values.
///
/// Accounts appearing in `System::Account::None` are routed to
/// `deleted_accounts`; the function then strips those addresses from any other
/// field so the final delta is self-consistent.
pub fn scan_overlay(main_storage_changes: &[(Vec<u8>, Option<Vec<u8>>)]) -> OverlayStateDelta {
	let sys_account_prefix: Vec<u8> =
		[twox_128(b"System").as_slice(), twox_128(b"Account").as_slice()].concat();
	let evm_codes_prefix: Vec<u8> =
		[twox_128(b"EVM").as_slice(), twox_128(b"AccountCodes").as_slice()].concat();
	let evm_storages_prefix: Vec<u8> =
		[twox_128(b"EVM").as_slice(), twox_128(b"AccountStorages").as_slice()].concat();

	let mut delta = OverlayStateDelta::default();

	for (key, value) in main_storage_changes {
		if key.starts_with(&sys_account_prefix) {
			if key.len() != SYS_KEY_LEN {
				continue;
			}
			let addr = H160::from_slice(&key[ADDR_OFFSET..ADDR_OFFSET + 20]);
			match value {
				Some(_) => {
					delta.changed_accounts.insert(addr);
				}
				None => {
					delta.deleted_accounts.insert(addr);
				}
			}
			continue;
		}
		if key.starts_with(&evm_codes_prefix) {
			if key.len() != CODES_KEY_LEN {
				continue;
			}
			let addr = H160::from_slice(&key[ADDR_OFFSET..ADDR_OFFSET + 20]);
			let code = match value {
				Some(bytes) => <Vec<u8>>::decode(&mut &bytes[..]).unwrap_or_default(),
				// EIP-7702 reset_delegation / remove_account_code only clears
				// code; the account itself is NOT deleted here.
				None => Vec::new(),
			};
			delta.code_updates.insert(addr, code);
			delta.changed_accounts.insert(addr);
			continue;
		}
		if key.starts_with(&evm_storages_prefix) {
			if key.len() != STORAGES_KEY_LEN {
				continue;
			}
			let addr = H160::from_slice(&key[ADDR_OFFSET..ADDR_OFFSET + 20]);
			let slot = H256::from_slice(&key[SLOT_OFFSET..SLOT_OFFSET + 32]);
			let val = match value {
				Some(bytes) => H256::decode(&mut &bytes[..]).unwrap_or_default(),
				// SSTORE(slot, 0) / reset_storage / clear_prefix.
				None => H256::zero(),
			};
			delta
				.storage_diff
				.entry(addr)
				.or_default()
				.insert(slot, val);
			delta.changed_accounts.insert(addr);
		}
	}

	// SELFDESTRUCT flows through `remove_account` which removes the
	// `System::Account` entry *and* fires `clear_prefix` on AccountStorages and
	// `remove` on AccountCodes. Dedupe so a deleted account doesn't also appear
	// in the changed / storage / code lists.
	for addr in delta.deleted_accounts.clone() {
		delta.changed_accounts.remove(&addr);
		delta.storage_diff.remove(&addr);
		delta.code_updates.remove(&addr);
	}

	delta
}

#[cfg(test)]
mod tests {
	use super::*;
	use parity_scale_codec::Encode;

	fn sys_account_key(addr: H160) -> Vec<u8> {
		use sp_core::hashing::blake2_128;
		let prefix: Vec<u8> = [twox_128(b"System").as_slice(), twox_128(b"Account").as_slice()]
			.concat();
		let hashed = blake2_128(addr.as_bytes());
		[
			prefix.as_slice(),
			hashed.as_slice(),
			addr.as_bytes(),
		]
		.concat()
	}

	fn evm_codes_key(addr: H160) -> Vec<u8> {
		use sp_core::hashing::blake2_128;
		let prefix: Vec<u8> = [
			twox_128(b"EVM").as_slice(),
			twox_128(b"AccountCodes").as_slice(),
		]
		.concat();
		let hashed = blake2_128(addr.as_bytes());
		[
			prefix.as_slice(),
			hashed.as_slice(),
			addr.as_bytes(),
		]
		.concat()
	}

	fn evm_storages_key(addr: H160, slot: H256) -> Vec<u8> {
		use sp_core::hashing::blake2_128;
		let prefix: Vec<u8> = [
			twox_128(b"EVM").as_slice(),
			twox_128(b"AccountStorages").as_slice(),
		]
		.concat();
		let addr_hash = blake2_128(addr.as_bytes());
		let slot_hash = blake2_128(slot.as_bytes());
		[
			prefix.as_slice(),
			addr_hash.as_slice(),
			addr.as_bytes(),
			slot_hash.as_slice(),
			slot.as_bytes(),
		]
		.concat()
	}

	#[test]
	fn system_account_some_goes_to_changed() {
		let addr = H160::repeat_byte(0xAA);
		let input = vec![(sys_account_key(addr), Some(vec![1, 2, 3]))];
		let d = scan_overlay(&input);
		assert!(d.changed_accounts.contains(&addr));
		assert!(d.deleted_accounts.is_empty());
	}

	#[test]
	fn system_account_none_goes_to_deleted() {
		let addr = H160::repeat_byte(0xBB);
		let input = vec![(sys_account_key(addr), None)];
		let d = scan_overlay(&input);
		assert!(d.deleted_accounts.contains(&addr));
		assert!(d.changed_accounts.is_empty());
	}

	#[test]
	fn account_codes_none_does_not_delete_account() {
		// EIP-7702 reset_delegation calls remove_account_code which only
		// clears AccountCodes/AccountCodesMetadata. The account itself
		// (System::Account) stays alive.
		let addr = H160::repeat_byte(0xCC);
		let input = vec![(evm_codes_key(addr), None)];
		let d = scan_overlay(&input);
		assert!(d.changed_accounts.contains(&addr));
		assert!(!d.deleted_accounts.contains(&addr));
		assert_eq!(d.code_updates.get(&addr), Some(&Vec::<u8>::new()));
	}

	#[test]
	fn account_codes_some_decodes_scale_vec_u8() {
		let addr = H160::repeat_byte(0xDD);
		let code = vec![0x60, 0x80, 0x60, 0x40];
		let scale_encoded = code.encode();
		let input = vec![(evm_codes_key(addr), Some(scale_encoded))];
		let d = scan_overlay(&input);
		assert!(d.changed_accounts.contains(&addr));
		assert_eq!(d.code_updates.get(&addr), Some(&code));
	}

	#[test]
	fn account_storages_some_decodes_scale_h256() {
		let addr = H160::repeat_byte(0xEE);
		let slot = H256::repeat_byte(0x11);
		let val = H256::repeat_byte(0x42);
		let scale_encoded = val.encode();
		let input = vec![(evm_storages_key(addr, slot), Some(scale_encoded))];
		let d = scan_overlay(&input);
		assert!(d.changed_accounts.contains(&addr));
		assert_eq!(
			d.storage_diff.get(&addr).and_then(|m| m.get(&slot)),
			Some(&val)
		);
	}

	#[test]
	fn account_storages_none_maps_to_zero() {
		// SSTORE(slot, 0) and reset_storage / clear_prefix land here.
		let addr = H160::repeat_byte(0xEF);
		let slot = H256::repeat_byte(0x22);
		let input = vec![(evm_storages_key(addr, slot), None)];
		let d = scan_overlay(&input);
		assert_eq!(
			d.storage_diff.get(&addr).and_then(|m| m.get(&slot)),
			Some(&H256::zero())
		);
	}

	#[test]
	fn selfdestruct_dedup_strips_storage_and_code() {
		// Scenario: SELFDESTRUCT on `addr` kills System::Account and
		// synchronously fires AccountCodes::remove + AccountStorages
		// clear_prefix. Deleted accounts must take priority — they must not
		// also appear as changed_accounts, storage_diff, or code_updates.
		let addr = H160::repeat_byte(0x99);
		let slot = H256::repeat_byte(0x33);
		let input = vec![
			(sys_account_key(addr), None),
			(evm_codes_key(addr), None),
			(evm_storages_key(addr, slot), None),
		];
		let d = scan_overlay(&input);
		assert!(d.deleted_accounts.contains(&addr));
		assert!(!d.changed_accounts.contains(&addr));
		assert!(!d.storage_diff.contains_key(&addr));
		assert!(!d.code_updates.contains_key(&addr));
	}

	#[test]
	fn unrelated_prefixes_are_ignored() {
		// A key under some other pallet prefix must not produce any delta.
		let unrelated_prefix: Vec<u8> = [
			twox_128(b"Balances").as_slice(),
			twox_128(b"Locks").as_slice(),
		]
		.concat();
		let mut key = unrelated_prefix;
		key.extend_from_slice(&[0u8; 36]);
		let input = vec![(key, Some(vec![1, 2, 3]))];
		let d = scan_overlay(&input);
		assert!(d.changed_accounts.is_empty());
		assert!(d.deleted_accounts.is_empty());
		assert!(d.storage_diff.is_empty());
		assert!(d.code_updates.is_empty());
	}

	#[test]
	fn wrong_length_keys_are_skipped_not_panicking() {
		// A System::Account prefix match with an impossible key length
		// must not panic or corrupt the delta.
		let sys_prefix: Vec<u8> =
			[twox_128(b"System").as_slice(), twox_128(b"Account").as_slice()].concat();
		let short = sys_prefix.clone(); // 32 bytes, below the 68 expected
		let input = vec![(short, Some(vec![0x00]))];
		let d = scan_overlay(&input);
		assert!(d.changed_accounts.is_empty());
	}
}
