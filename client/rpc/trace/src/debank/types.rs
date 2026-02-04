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

//! Internal Debank types for state diff encoding.
//! The main Debank output types are defined in moonbeam-rpc-core-types::debank.

// Re-export types from moonbeam-client-evm-tracing to avoid duplication.
// These types are used for state diff encoding and must be consistent.
pub use moonbeam_client_evm_tracing::types::block::{
	AccountStorageDiff, BlockStorageDiff, IndexValuePair, NewAccount, NewCode,
};
