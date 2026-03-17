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
use ethereum_types::H256;

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
