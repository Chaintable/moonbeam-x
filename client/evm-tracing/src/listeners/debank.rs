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

//! Debank trace listener - collects call traces, events, and state diff in one pass.
//! This is based on the Go implementation in call_tracer.go.

use ethereum_types::{H160, H256, U256};
use evm_tracing_events::{
	runtime::{Capture, ExitError, ExitReason},
	Event, EvmEvent, GasometerEvent, Listener as ListenerT, RuntimeEvent, StepEventFilter,
};
use std::collections::{HashMap, HashSet};

/// Tracing version based on runtime capabilities.
#[derive(Debug, Clone, Copy)]
enum TracingVersion {
	/// Older runtimes without TransactX/Exit events.
	/// Frame exits only come from RuntimeEvent::StepResult(Capture::Exit).
	Legacy,
	/// Modern runtimes with TransactX/Exit events.
	/// EvmEvent::Exit is always emitted; StepResult(Capture::Exit) may also fire.
	EarlyTransact,
}

/// Call type for Debank tracing (includes Create and Suicide)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebankCallType {
	Call,
	CallCode,
	DelegateCall,
	StaticCall,
	Create,
	Suicide,
}

/// Call frame representing a single call/create in the trace
#[derive(Debug, Clone)]
pub struct CallFrame {
	/// Call type (CALL, CREATE, STATICCALL, etc.)
	pub call_type: DebankCallType,
	/// Caller address
	pub from: H160,
	/// Target address (to or created contract)
	pub to: H160,
	/// Gas provided at start of this call
	pub gas: u64,
	/// Gas used by this call
	pub gas_used: u64,
	/// Gas remaining (updated during execution, used to calculate gas_used on exit)
	gas_remaining: u64,
	/// Actual gas at frame start, captured from the first gasometer snapshot.
	/// Used to set `gas` (the reported gas limit) on exit.
	start_gas: Option<u64>,
	/// Input data
	pub input: Vec<u8>,
	/// Output data
	pub output: Vec<u8>,
	/// Value transferred
	pub value: U256,
	/// Error message if failed
	pub error: String,
	/// Position in parent trace (for calculating ID)
	pub pos_in_parent_trace: usize,
	/// Whether this frame had a direct storage change (SSTORE)
	pub self_storage_change: bool,
	/// Whether this frame or any subcall had storage change
	pub storage_change: bool,
	/// Whether this call failed
	pub failed: bool,
	/// Whether parent call failed
	pub parent_failed: bool,
	/// Subcalls
	pub calls: Vec<CallFrame>,
	/// Events/logs emitted in this call
	pub logs: Vec<EventLog>,
	/// Trace address (position in call tree)
	pub trace_address: Vec<usize>,
	/// Whether this frame has been processed by capture_exit (output/gas computed).
	exited: bool,
	/// Whether this is a precompile subcall (may not have matching Exit event)
	pub is_precompile: bool,
	/// Whether this precompile has had a subcall (only meaningful for precompile frames).
	/// Used to determine if an Exit event belongs to this precompile or its parent.
	had_subcall: bool,
}

impl CallFrame {
	fn new(
		call_type: DebankCallType,
		from: H160,
		to: H160,
		gas: u64,
		value: U256,
		input: Vec<u8>,
	) -> Self {
		Self {
			call_type,
			from,
			to,
			gas,
			gas_used: 0,
			gas_remaining: gas,
			start_gas: None,
			input,
			output: Vec::new(),
			value,
			error: String::new(),
			pos_in_parent_trace: 0,
			self_storage_change: false,
			storage_change: false,
			failed: false,
			parent_failed: false,
			calls: Vec::new(),
			logs: Vec::new(),
			trace_address: Vec::new(),
			exited: false,
			is_precompile: false,
			had_subcall: false,
		}
	}

	fn process_output(&mut self, output: Vec<u8>, reason: &ExitReason) {
		self.exited = true;
		match reason {
			ExitReason::Succeed(_) => {
				self.output = output;
			}
			ExitReason::Error(error) => {
				self.failed = true;
				self.error = error_message(error);
				if self.call_type == DebankCallType::Create {
					self.to = H160::zero();
				}
			}
			ExitReason::Revert(_) => {
				self.failed = true;
				self.error = "execution reverted".to_string();
				self.output = output;
			}
			ExitReason::Fatal(_) => {
				self.failed = true;
				self.error = "fatal error".to_string();
			}
		}
	}
}

/// Event log captured during execution
#[derive(Debug, Clone)]
pub struct EventLog {
	/// Contract address that emitted the log
	pub address: H160,
	/// Log topics
	pub topics: Vec<H256>,
	/// Log data
	pub data: Vec<u8>,
	/// Position in parent trace
	pub position: usize,
	/// Log index in transaction
	pub log_index: usize,
}

/// Debank listener for collecting traces, events, and state diff
#[derive(Debug)]
pub struct Listener {
	/// Call stack during tracing
	callstack: Vec<CallFrame>,
	/// Transaction cost (intrinsic gas for gas calculation)
	transaction_cost: u64,

	/// Completed call frames (one per transaction when tracing blocks)
	pub completed_frames: Vec<CallFrame>,

	// State diff tracking
	/// Accounts created in this block
	pub created_accounts: HashSet<H160>,
	/// Accounts deleted (selfdestruct)
	pub deleted_accounts: HashSet<H160>,
	/// Accounts with balance/nonce changes
	pub touched_accounts: HashSet<H160>,
	/// Storage changes: address -> (slot -> value)
	pub storage_changes: HashMap<H160, HashMap<H256, H256>>,
	/// Contracts with storage changes
	pub storage_contracts: HashSet<H160>,

	/// Call type for next context
	call_type: Option<DebankCallType>,
	/// Skip next context (after TransactX)
	skip_next_context: bool,
	/// First transaction flag
	first_transaction: bool,
	/// Global log index counter
	global_log_index: usize,

	/// Tracing version (Legacy or EarlyTransact)
	version: TracingVersion,
	/// Count of exits already handled by StepResult(Capture::Exit),
	/// preventing double-pop when the corresponding EvmEvent::Exit follows.
	/// Supports multiple pending exits in case they arrive in batch.
	step_result_exit_count: usize,
	/// True if only RecordTransaction was received; handles edge case where
	/// transaction cannot pay for its own data cost in Legacy mode.
	record_transaction_event_only: bool,
}

impl Default for Listener {
	fn default() -> Self {
		Self {
			callstack: Vec::new(),
			transaction_cost: 0,
			completed_frames: Vec::new(),
			created_accounts: HashSet::new(),
			deleted_accounts: HashSet::new(),
			touched_accounts: HashSet::new(),
			storage_changes: HashMap::new(),
			storage_contracts: HashSet::new(),
			call_type: None,
			skip_next_context: false,
			first_transaction: true,
			global_log_index: 0,
			version: TracingVersion::Legacy,
			step_result_exit_count: 0,
			record_transaction_event_only: false,
		}
	}
}

impl Listener {
	pub fn new() -> Self {
		Self::default()
	}

	pub fn using<R, F: FnOnce() -> R>(&mut self, f: F) -> R {
		evm_tracing_events::using(self, f)
	}

	/// Flush precompile frames that don't have a matching Exit event.
	/// This handles the case where precompile returns via Capture::Exit (no Exit event).
	///
	/// If `expected_caller` is Some, we keep the topmost precompile frame if its `to` address
	/// matches the expected_caller (indicating it's a Capture::Trap path where the precompile
	/// initiated the subcall).
	fn flush_pending_precompiles(&mut self, expected_caller: Option<H160>) {
		// Pop precompile frames from the top of callstack that don't match expected_caller
		while self.callstack.len() > 1 {
			let frame = self.callstack.last().unwrap();
			if !frame.is_precompile {
				break;
			}
			// If expected_caller matches the precompile's address, it's Capture::Trap
			// (the precompile initiated this call), so we keep it
			if let Some(caller) = expected_caller {
				if frame.to == caller {
					break;
				}
			}
			self.pop_frame_to_parent();
		}
	}

	/// Mark the topmost precompile frame as having a subcall if the caller matches.
	/// This is used to distinguish Capture::Trap (has subcall) from Capture::Exit (no subcall).
	fn mark_precompile_has_subcall(&mut self, caller: H160) {
		if let Some(frame) = self.callstack.last_mut() {
			if frame.is_precompile && frame.to == caller {
				frame.had_subcall = true;
			}
		}
	}

	/// Pop the top frame from callstack and add it to parent's calls.
	fn pop_frame_to_parent(&mut self) {
		if let Some(mut frame) = self.callstack.pop() {
			frame.pos_in_parent_trace = if let Some(parent) = self.callstack.last() {
				parent.calls.len() + parent.logs.len()
			} else {
				0
			};
			if let Some(parent) = self.callstack.last_mut() {
				parent.calls.push(frame);
			}
		}
	}

	/// Calculate the trace_address for the next child frame.
	fn next_trace_address(&self) -> Vec<usize> {
		if let Some(parent) = self.callstack.last() {
			let mut ta = parent.trace_address.clone();
			ta.push(parent.calls.len());
			ta
		} else {
			vec![]
		}
	}

	/// Flush precompile frames that haven't had any subcalls.
	/// This handles the case where a precompile returned via Capture::Exit (no Exit event)
	/// and the next event is an Exit for the parent call.
	fn flush_precompiles_without_subcalls(&mut self) {
		while self.callstack.len() > 1 {
			let frame = self.callstack.last().unwrap();
			// Only flush precompile frames that haven't had subcalls
			if !(frame.is_precompile && !frame.had_subcall) {
				break;
			}
			self.pop_frame_to_parent();
		}
	}

	/// Called at the end of each transaction when tracing a block
	pub fn finish_transaction(&mut self) {
		// Flush any pending precompile frames that didn't get an Exit event
		self.flush_pending_precompiles(None);

		// Drain callstack; keep only the root frame (first), discard inner leftovers.
		let mut callstack = Vec::new();
		core::mem::swap(&mut self.callstack, &mut callstack);

		if let Some(mut root_frame) = callstack.into_iter().next() {
			// If root frame was never processed by capture_exit (Legacy early exit),
			// calculate gas and mark as error.
			if !root_frame.exited {
				root_frame.failed = true;
				root_frame.error =
					"early exit (out of gas, stack overflow, direct call to precompile, ...)"
						.to_string();
				if let Some(sg) = root_frame.start_gas {
					root_frame.gas = sg;
				}
				root_frame.gas_used = root_frame.gas.saturating_sub(root_frame.gas_remaining);
			}

			// Set parent_failed flags recursively
			set_parent_failed(&mut root_frame, false);
			// Calculate storage_change flags
			set_storage_change(&mut root_frame, &mut self.storage_contracts);
			self.completed_frames.push(root_frame);
		} else if self.record_transaction_event_only {
			// Transaction couldn't pay for its own data cost (Legacy mode edge case).
			// No frames were ever created; produce a dummy error frame.
			let mut frame = CallFrame::new(
				DebankCallType::Call,
				H160::zero(),
				H160::zero(),
				0,
				U256::zero(),
				Vec::new(),
			);
			frame.failed = true;
			frame.error = "transaction could not pay its own data cost".to_string();
			self.completed_frames.push(frame);
		}

		// Clear state for next transaction
		self.callstack.clear();
		self.skip_next_context = false;
		self.call_type = None;
		self.step_result_exit_count = 0;
		self.record_transaction_event_only = false;
	}

	fn gasometer_event(&mut self, event: GasometerEvent) {
		match event {
			GasometerEvent::RecordCost { snapshot, .. }
			| GasometerEvent::RecordDynamicCost { snapshot, .. }
			| GasometerEvent::RecordStipend { snapshot, .. } => {
				// Update gas_remaining for the current frame
				if let Some(frame) = self.callstack.last_mut() {
					if frame.start_gas.is_none() {
						frame.start_gas = Some(snapshot.gas());
					}
					frame.gas_remaining = snapshot.gas();
				}
			}
			GasometerEvent::RecordTransaction { cost, .. } => {
				self.transaction_cost = cost;
				self.record_transaction_event_only = true;
			}
			_ => {}
		}
	}

	fn runtime_event(&mut self, event: RuntimeEvent) {
		match event {
			RuntimeEvent::StepResult {
				result: Err(Capture::Trap(opcode)),
				..
			} => {
				if let Some(call_type) = call_type_from_opcode(&opcode) {
					self.call_type = Some(call_type);
				}
			}
			RuntimeEvent::StepResult {
				result: Err(Capture::Exit(reason)),
				return_value,
			} => {
				self.flush_precompiles_without_subcalls();
				match self.version {
					TracingVersion::Legacy => {
						// In Legacy mode, this is the only exit event; handle directly.
						self.capture_exit(&reason, return_value);
					}
					TracingVersion::EarlyTransact => {
						// In EarlyTransact mode, EvmEvent::Exit will also fire for this
						// same frame. Process with StepResult's data (more accurate) and
						// increment counter so EvmEvent::Exit skips the redundant pop.
						self.capture_exit(&reason, return_value);
						self.step_result_exit_count += 1;
					}
				}
			}
			RuntimeEvent::SStore {
				address,
				index,
				value,
			} => {
				// Flush precompile frames - SSTORE means parent is continuing
				// (standard precompiles don't do SSTORE)
				self.flush_pending_precompiles(None);

				// Track storage changes
				self.storage_changes
					.entry(address)
					.or_default()
					.insert(index, value);

				// Mark current frame as having storage change
				if let Some(frame) = self.callstack.last_mut() {
					frame.self_storage_change = true;
					frame.storage_change = true;
				}
			}
			_ => {}
		}
	}

	fn evm_event(&mut self, event: EvmEvent) {
		match event {
			EvmEvent::TransactCall {
				caller,
				address,
				value,
				data,
				gas_limit,
			} => {
				self.version = TracingVersion::EarlyTransact;
				self.record_transaction_event_only = false;

				self.touched_accounts.insert(caller);
				if !value.is_zero() {
					self.touched_accounts.insert(address);
				}

				self.callstack.push(CallFrame::new(
					DebankCallType::Call,
					caller,
					address,
					gas_limit,
					value,
					data,
				));
				self.skip_next_context = true;
			}

			EvmEvent::TransactCreate {
				caller,
				value,
				init_code,
				gas_limit,
				address,
			}
			| EvmEvent::TransactCreate2 {
				caller,
				value,
				init_code,
				gas_limit,
				address,
				..
			} => {
				self.version = TracingVersion::EarlyTransact;
				self.record_transaction_event_only = false;

				self.touched_accounts.insert(caller);
				self.created_accounts.insert(address);
				self.touched_accounts.insert(address);

				self.callstack.push(CallFrame::new(
					DebankCallType::Create,
					caller,
					address,
					gas_limit,
					value,
					init_code,
				));
				self.skip_next_context = true;
			}

			EvmEvent::Call {
				code_address,
				input,
				is_static,
				context,
				target_gas,
				..
			} => {
				self.record_transaction_event_only = false;

				// Flush precompile frames that don't match this caller
				// (they returned via Capture::Exit without an Exit event)
				self.flush_pending_precompiles(Some(context.caller));
				// Mark matching precompile as having a subcall (Capture::Trap path)
				self.mark_precompile_has_subcall(context.caller);

				if !context.apparent_value.is_zero() {
					self.touched_accounts.insert(context.caller);
					self.touched_accounts.insert(code_address);
				}

				if !self.skip_next_context {
					let call_type = match (self.call_type.take(), is_static) {
						(None, true) => DebankCallType::StaticCall,
						(None, false) => DebankCallType::Call,
						(Some(ct), _) => ct,
					};

					let (trace_address, from) = if let Some(parent) = self.callstack.last_mut() {
						let mut ta = parent.trace_address.clone();
						ta.push(parent.calls.len());
						(ta, parent.to)
					} else {
						(vec![], context.caller)
					};

					let gas = target_gas.unwrap_or(0);
					let mut frame = CallFrame::new(
						call_type,
						from,
						code_address,
						gas,
						context.apparent_value,
						input.to_vec(),
					);
					frame.trace_address = trace_address;
					self.callstack.push(frame);
				} else {
					self.skip_next_context = false;
				}
			}

			EvmEvent::Create {
				caller,
				address,
				value,
				init_code,
				target_gas,
				..
			} => {
				self.record_transaction_event_only = false;

				// Flush precompile frames that don't match this caller
				self.flush_pending_precompiles(Some(caller));
				// Mark matching precompile as having a subcall (Capture::Trap path)
				self.mark_precompile_has_subcall(caller);

				if !value.is_zero() {
					self.touched_accounts.insert(caller);
				}
				self.created_accounts.insert(address);
				self.touched_accounts.insert(address);

				if !self.skip_next_context {
					let gas = target_gas.unwrap_or(0);
					let mut frame = CallFrame::new(
						DebankCallType::Create,
						caller,
						address,
						gas,
						value,
						init_code.to_vec(),
					);
					frame.trace_address = self.next_trace_address();
					self.callstack.push(frame);
				} else {
					self.skip_next_context = false;
				}
			}

			EvmEvent::Suicide {
				address,
				target,
				balance,
			} => {
				// Flush precompile frames - SELFDESTRUCT means parent is continuing
				self.flush_pending_precompiles(None);

				self.deleted_accounts.insert(address);
				self.touched_accounts.insert(address);
				if !balance.is_zero() {
					self.touched_accounts.insert(target);
				}

				// Create a suicide call frame
				// Note: For Suicide, `from` is the self-destructing contract,
				// `to` is the beneficiary receiving the balance (different from Call/Create semantics)
				let trace_address = self.next_trace_address();
				if let Some(parent) = self.callstack.last_mut() {
					let mut frame = CallFrame::new(
						DebankCallType::Suicide,
						address, // from = self-destructing contract
						target,  // to = beneficiary
						0,
						balance,
						Vec::new(),
					);
					frame.trace_address = trace_address;
					frame.pos_in_parent_trace = parent.calls.len() + parent.logs.len();
					parent.calls.push(frame);
				}
			}

			EvmEvent::Exit {
				reason,
				return_value,
			} => {
				self.record_transaction_event_only = false;

				if self.step_result_exit_count > 0 {
					// StepResult already processed this exit; skip to avoid double-pop.
					self.step_result_exit_count -= 1;
				} else {
					// StepResult was skipped (e.g. precompile call); handle normally.
					self.flush_precompiles_without_subcalls();
					self.capture_exit(&reason, return_value);
				}
			}

			EvmEvent::PrecompileSubcall {
				code_address,
				input,
				target_gas,
				context,
				..
			} => {
				// Flush precompile frames that don't match this caller
				self.flush_pending_precompiles(Some(context.caller));
				// Mark matching precompile as having a subcall (nested precompile case)
				self.mark_precompile_has_subcall(context.caller);

				// Track touched accounts if value transfer
				if !context.apparent_value.is_zero() {
					self.touched_accounts.insert(context.caller);
					self.touched_accounts.insert(code_address);
				}

				// Create precompile frame
				let gas = target_gas.unwrap_or(0);
				let mut frame = CallFrame::new(
					DebankCallType::Call,
					context.caller,
					code_address,
					gas,
					context.apparent_value,
					input,
				);
				frame.trace_address = self.next_trace_address();
				frame.is_precompile = true;
				self.callstack.push(frame);
			}

			EvmEvent::Log {
				address,
				topics,
				data,
			} => {
				// Flush precompile frames - LOG means parent is continuing
				// (standard precompiles don't emit logs)
				self.flush_pending_precompiles(None);

				if let Some(frame) = self.callstack.last_mut() {
					let position = frame.calls.len() + frame.logs.len();
					frame.logs.push(EventLog {
						address,
						topics,
						data,
						position,
						log_index: self.global_log_index,
					});
					self.global_log_index += 1;
				}
			}
		}
	}

	fn capture_exit(&mut self, reason: &ExitReason, return_value: Vec<u8>) {
		let size = self.callstack.len();
		if size <= 1 {
			// Root frame exit - just process output and calculate gas_used
			if let Some(frame) = self.callstack.last_mut() {
				// Save address before process_output (which may set to = zero on Create failure)
				let created_addr = if frame.call_type == DebankCallType::Create {
					Some(frame.to)
				} else {
					None
				};

				frame.process_output(return_value, reason);
				// Use actual start gas as gas limit
				if let Some(sg) = frame.start_gas {
					frame.gas = sg;
				}
				// Calculate gas_used for root frame (pure execution gas, no intrinsic cost)
				frame.gas_used = frame.gas.saturating_sub(frame.gas_remaining);

				// If Create failed, remove from created_accounts
				if frame.failed {
					if let Some(addr) = created_addr {
						self.created_accounts.remove(&addr);
					}
				}
			}
			return;
		}

		// Pop the call
		if let Some(mut call) = self.callstack.pop() {
			// Save address before process_output (which may set to = zero on Create failure)
			let created_addr = if call.call_type == DebankCallType::Create {
				Some(call.to)
			} else {
				None
			};

			call.process_output(return_value, reason);

			// Use actual start gas as gas limit
			if let Some(sg) = call.start_gas {
				call.gas = sg;
			}
			// Calculate gas_used = initial_gas - remaining_gas
			call.gas_used = call.gas.saturating_sub(call.gas_remaining);

			// If Create failed, remove from created_accounts
			if call.failed {
				if let Some(addr) = created_addr {
					self.created_accounts.remove(&addr);
				}
			}

			call.pos_in_parent_trace = if let Some(parent) = self.callstack.last() {
				parent.calls.len() + parent.logs.len()
			} else {
				0
			};

			// Nest into parent
			if let Some(parent) = self.callstack.last_mut() {
				parent.calls.push(call);
			}
		}
	}
}

impl ListenerT for Listener {
	fn event(&mut self, event: Event) {
		match event {
			Event::Gasometer(e) => self.gasometer_event(e),
			Event::Runtime(e) => self.runtime_event(e),
			Event::Evm(e) => self.evm_event(e),
			Event::CallListNew() => {
				if !self.first_transaction {
					self.finish_transaction();
				} else {
					self.first_transaction = false;
				}
			}
		}
	}

	fn step_event_filter(&self) -> StepEventFilter {
		StepEventFilter {
			enable_memory: false,
			enable_stack: false,
		}
	}
}

/// Recursively set parent_failed flag
fn set_parent_failed(frame: &mut CallFrame, parent_failed: bool) {
	let failed = frame.failed || parent_failed;
	for child in &mut frame.calls {
		child.parent_failed = failed;
		set_parent_failed(child, failed);
	}
}

/// Recursively calculate storage_change and track storage contracts
fn set_storage_change(frame: &mut CallFrame, storage_contracts: &mut HashSet<H160>) {
	// Track contracts with direct storage changes
	if frame.self_storage_change {
		match frame.call_type {
			DebankCallType::DelegateCall => {
				// For delegate call, the storage change happens on the caller
				storage_contracts.insert(frame.from);
			}
			_ => {
				storage_contracts.insert(frame.to);
			}
		}
	}

	// Recursively process children
	let mut sub_storage_change = false;
	for child in &mut frame.calls {
		set_storage_change(child, storage_contracts);
		if child.storage_change && !child.failed {
			sub_storage_change = true;
		}
	}

	if sub_storage_change {
		frame.storage_change = true;
	}
}

fn error_message(error: &ExitError) -> String {
	match error {
		ExitError::StackUnderflow => "stack underflow",
		ExitError::StackOverflow => "stack overflow",
		ExitError::InvalidJump => "invalid jump",
		ExitError::InvalidRange => "invalid range",
		ExitError::DesignatedInvalid => "designated invalid",
		ExitError::CallTooDeep => "call too deep",
		ExitError::CreateCollision => "create collision",
		ExitError::CreateContractLimit => "create contract limit",
		ExitError::OutOfOffset => "out of offset",
		ExitError::OutOfGas => "out of gas",
		ExitError::OutOfFund => "out of funds",
		ExitError::Other(err) => err,
		_ => "unexpected error",
	}
	.to_string()
}

fn call_type_from_opcode(opcode: &[u8]) -> Option<DebankCallType> {
	// Opcode bytes for CALL variants
	match opcode.first() {
		Some(0xF1) => Some(DebankCallType::Call),         // CALL
		Some(0xF2) => Some(DebankCallType::CallCode),     // CALLCODE
		Some(0xF4) => Some(DebankCallType::DelegateCall), // DELEGATECALL
		Some(0xFA) => Some(DebankCallType::StaticCall),   // STATICCALL
		_ => None,
	}
}

#[cfg(test)]
#[allow(unused)]
mod tests {
	use super::*;
	use evm::ExitRevert;
	use evm_tracing_events::{
		evm::CreateScheme,
		gasometer::Snapshot,
		runtime::{Capture, ExitSucceed, Memory, Stack},
		Context as EvmContext,
	};

	// Test event type enums
	enum TestEvmEvent {
		Call,
		Create,
		Suicide,
		Exit,
		TransactCall,
		TransactCreate,
		TransactCreate2,
		Log,
		PrecompileSubcall,
	}

	enum TestRuntimeEvent {
		Step,
		StepResult,
		StepResultExit,
		SStore,
	}

	enum TestGasometerEvent {
		RecordCost,
		RecordTransaction,
	}

	// Test helper functions
	fn test_context() -> EvmContext {
		EvmContext {
			address: H160::from_low_u64_be(1),
			caller: H160::from_low_u64_be(2),
			apparent_value: U256::zero(),
		}
	}

	fn test_context_with_caller(caller: H160) -> EvmContext {
		EvmContext {
			address: H160::from_low_u64_be(1),
			caller,
			apparent_value: U256::zero(),
		}
	}

	fn test_create_scheme() -> CreateScheme {
		CreateScheme::Legacy {
			caller: H160::default(),
		}
	}

	fn test_stack() -> Option<Stack> {
		None
	}

	fn test_memory() -> Option<Memory> {
		None
	}

	fn test_snapshot() -> Snapshot {
		Snapshot {
			gas_limit: 1000u64,
			memory_gas: 0u64,
			used_gas: 100u64,
			refunded_gas: 0i64,
		}
	}

	fn test_snapshot_with_gas(gas_limit: u64, used_gas: u64) -> Snapshot {
		Snapshot {
			gas_limit,
			memory_gas: 0u64,
			used_gas,
			refunded_gas: 0i64,
		}
	}

	fn test_emit_evm_event(
		event_type: TestEvmEvent,
		is_static: bool,
		exit_reason: Option<ExitReason>,
	) -> EvmEvent {
		match event_type {
			TestEvmEvent::Call => EvmEvent::Call {
				code_address: H160::from_low_u64_be(100),
				transfer: None,
				input: Vec::new(),
				target_gas: Some(10000),
				is_static,
				context: test_context(),
			},
			TestEvmEvent::Create => EvmEvent::Create {
				caller: H160::from_low_u64_be(2),
				address: H160::from_low_u64_be(200),
				scheme: test_create_scheme(),
				value: U256::zero(),
				init_code: Vec::new(),
				target_gas: Some(10000),
			},
			TestEvmEvent::Suicide => EvmEvent::Suicide {
				address: H160::from_low_u64_be(100),
				target: H160::from_low_u64_be(300),
				balance: U256::from(1000),
			},
			TestEvmEvent::Exit => EvmEvent::Exit {
				reason: exit_reason.unwrap_or(ExitReason::Succeed(ExitSucceed::Returned)),
				return_value: Vec::new(),
			},
			TestEvmEvent::TransactCall => EvmEvent::TransactCall {
				caller: H160::from_low_u64_be(1),
				address: H160::from_low_u64_be(100),
				value: U256::zero(),
				data: Vec::new(),
				gas_limit: 21000u64,
			},
			TestEvmEvent::TransactCreate => EvmEvent::TransactCreate {
				caller: H160::from_low_u64_be(1),
				value: U256::zero(),
				init_code: Vec::new(),
				gas_limit: 21000u64,
				address: H160::from_low_u64_be(200),
			},
			TestEvmEvent::TransactCreate2 => EvmEvent::TransactCreate2 {
				caller: H160::from_low_u64_be(1),
				value: U256::zero(),
				init_code: Vec::new(),
				salt: H256::default(),
				gas_limit: 21000u64,
				address: H160::from_low_u64_be(200),
			},
			TestEvmEvent::Log => EvmEvent::Log {
				address: H160::from_low_u64_be(100),
				topics: vec![H256::from_low_u64_be(1)],
				data: vec![1, 2, 3],
			},
			TestEvmEvent::PrecompileSubcall => EvmEvent::PrecompileSubcall {
				code_address: H160::from_low_u64_be(10), // Precompile address
				transfer: None,
				input: Vec::new(),
				target_gas: Some(5000),
				is_static: false,
				context: test_context(),
			},
		}
	}

	fn test_emit_evm_event_with_caller(
		event_type: TestEvmEvent,
		caller: H160,
		code_address: H160,
	) -> EvmEvent {
		match event_type {
			TestEvmEvent::Call => EvmEvent::Call {
				code_address,
				transfer: None,
				input: Vec::new(),
				target_gas: Some(10000),
				is_static: false,
				context: test_context_with_caller(caller),
			},
			TestEvmEvent::PrecompileSubcall => EvmEvent::PrecompileSubcall {
				code_address,
				transfer: None,
				input: Vec::new(),
				target_gas: Some(5000),
				is_static: false,
				context: test_context_with_caller(caller),
			},
			_ => test_emit_evm_event(event_type, false, None),
		}
	}

	fn test_emit_runtime_event(event_type: TestRuntimeEvent) -> RuntimeEvent {
		match event_type {
			TestRuntimeEvent::Step => RuntimeEvent::Step {
				context: test_context(),
				opcode: Vec::new(),
				position: Ok(0u64),
				stack: test_stack(),
				memory: test_memory(),
			},
			TestRuntimeEvent::StepResult => RuntimeEvent::StepResult {
				result: Ok(()),
				return_value: Vec::new(),
			},
			TestRuntimeEvent::StepResultExit => RuntimeEvent::StepResult {
				result: Err(Capture::Exit(ExitReason::Succeed(ExitSucceed::Returned))),
				return_value: Vec::new(),
			},
			TestRuntimeEvent::SStore => RuntimeEvent::SStore {
				address: H160::from_low_u64_be(100),
				index: H256::from_low_u64_be(1),
				value: H256::from_low_u64_be(42),
			},
		}
	}

	fn test_emit_runtime_event_with_opcode(opcode: u8) -> RuntimeEvent {
		RuntimeEvent::StepResult {
			result: Err(Capture::Trap(vec![opcode])),
			return_value: Vec::new(),
		}
	}

	fn test_emit_gasometer_event(event_type: TestGasometerEvent) -> GasometerEvent {
		match event_type {
			TestGasometerEvent::RecordCost => GasometerEvent::RecordCost {
				cost: 100u64,
				snapshot: test_snapshot(),
			},
			TestGasometerEvent::RecordTransaction => GasometerEvent::RecordTransaction {
				cost: 21000u64,
				snapshot: test_snapshot(),
			},
		}
	}

	// Helper functions to emit events to listener
	fn do_transact_call_event(listener: &mut Listener) {
		listener.evm_event(test_emit_evm_event(TestEvmEvent::TransactCall, false, None));
	}

	fn do_transact_create_event(listener: &mut Listener) {
		listener.evm_event(test_emit_evm_event(
			TestEvmEvent::TransactCreate,
			false,
			None,
		));
	}

	fn do_gasometer_event(listener: &mut Listener) {
		listener.gasometer_event(test_emit_gasometer_event(
			TestGasometerEvent::RecordTransaction,
		));
	}

	fn do_gasometer_cost_event(listener: &mut Listener) {
		listener.gasometer_event(test_emit_gasometer_event(TestGasometerEvent::RecordCost));
	}

	fn do_exit_event(listener: &mut Listener) {
		listener.evm_event(test_emit_evm_event(
			TestEvmEvent::Exit,
			false,
			Some(ExitReason::Succeed(ExitSucceed::Returned)),
		));
	}

	fn do_exit_error_event(listener: &mut Listener) {
		listener.evm_event(test_emit_evm_event(
			TestEvmEvent::Exit,
			false,
			Some(ExitReason::Error(ExitError::OutOfGas)),
		));
	}

	fn do_exit_revert_event(listener: &mut Listener) {
		listener.evm_event(test_emit_evm_event(
			TestEvmEvent::Exit,
			false,
			Some(ExitReason::Revert(ExitRevert::Reverted)),
		));
	}

	fn do_evm_call_event(listener: &mut Listener) {
		listener.evm_event(test_emit_evm_event(TestEvmEvent::Call, false, None));
	}

	fn do_evm_static_call_event(listener: &mut Listener) {
		listener.evm_event(test_emit_evm_event(TestEvmEvent::Call, true, None));
	}

	fn do_evm_create_event(listener: &mut Listener) {
		listener.evm_event(test_emit_evm_event(TestEvmEvent::Create, false, None));
	}

	fn do_evm_suicide_event(listener: &mut Listener) {
		listener.evm_event(test_emit_evm_event(TestEvmEvent::Suicide, false, None));
	}

	fn do_evm_log_event(listener: &mut Listener) {
		listener.evm_event(test_emit_evm_event(TestEvmEvent::Log, false, None));
	}

	fn do_precompile_subcall_event(listener: &mut Listener) {
		listener.evm_event(test_emit_evm_event(
			TestEvmEvent::PrecompileSubcall,
			false,
			None,
		));
	}

	fn do_precompile_subcall_event_with_caller(
		listener: &mut Listener,
		caller: H160,
		code_address: H160,
	) {
		listener.evm_event(test_emit_evm_event_with_caller(
			TestEvmEvent::PrecompileSubcall,
			caller,
			code_address,
		));
	}

	fn do_evm_call_event_with_caller(listener: &mut Listener, caller: H160, code_address: H160) {
		listener.evm_event(test_emit_evm_event_with_caller(
			TestEvmEvent::Call,
			caller,
			code_address,
		));
	}

	fn do_runtime_step_event(listener: &mut Listener) {
		listener.runtime_event(test_emit_runtime_event(TestRuntimeEvent::Step));
	}

	fn do_runtime_step_result_event(listener: &mut Listener) {
		listener.runtime_event(test_emit_runtime_event(TestRuntimeEvent::StepResult));
	}

	fn do_runtime_step_result_exit_event(listener: &mut Listener) {
		listener.runtime_event(test_emit_runtime_event(TestRuntimeEvent::StepResultExit));
	}

	fn do_runtime_sstore_event(listener: &mut Listener) {
		listener.runtime_event(test_emit_runtime_event(TestRuntimeEvent::SStore));
	}

	fn do_runtime_call_opcode_event(listener: &mut Listener) {
		listener.runtime_event(test_emit_runtime_event_with_opcode(0xF1)); // CALL
	}

	fn do_runtime_delegatecall_opcode_event(listener: &mut Listener) {
		listener.runtime_event(test_emit_runtime_event_with_opcode(0xF4)); // DELEGATECALL
	}

	fn do_runtime_staticcall_opcode_event(listener: &mut Listener) {
		listener.runtime_event(test_emit_runtime_event_with_opcode(0xFA)); // STATICCALL
	}

	// ============ Basic Call Tests ============

	#[test]
	fn basic_transact_call() {
		let mut listener = Listener::default();
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_exit_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		assert_eq!(listener.completed_frames[0].call_type, DebankCallType::Call);
		assert!(!listener.completed_frames[0].failed);
	}

	#[test]
	fn basic_transact_call_with_error() {
		let mut listener = Listener::default();
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_exit_error_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		assert!(listener.completed_frames[0].failed);
		assert_eq!(listener.completed_frames[0].error, "out of gas");
	}

	#[test]
	fn basic_transact_call_with_revert() {
		let mut listener = Listener::default();
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_exit_revert_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		assert!(listener.completed_frames[0].failed);
		assert_eq!(listener.completed_frames[0].error, "execution reverted");
	}

	// ============ Basic Create Tests ============

	#[test]
	fn basic_transact_create() {
		let mut listener = Listener::default();
		do_transact_create_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_exit_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		assert_eq!(
			listener.completed_frames[0].call_type,
			DebankCallType::Create
		);
		assert!(!listener.completed_frames[0].failed);
		assert!(listener.created_accounts.contains(&H160::from_low_u64_be(200)));
	}

	#[test]
	fn basic_transact_create_with_error() {
		let mut listener = Listener::default();
		do_transact_create_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_exit_error_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		assert!(listener.completed_frames[0].failed);
		// Created account should be removed on failure
		assert!(!listener.created_accounts.contains(&H160::from_low_u64_be(200)));
	}

	// ============ Nested Call Tests ============

	#[test]
	fn nested_call() {
		let mut listener = Listener::default();
		// Main call
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);
		// Nested call
		do_evm_call_event(&mut listener);
		do_exit_event(&mut listener);
		// Main exit
		do_exit_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		assert_eq!(listener.completed_frames[0].calls.len(), 1);
		assert_eq!(
			listener.completed_frames[0].calls[0].trace_address,
			vec![0]
		);
	}

	#[test]
	fn deeply_nested_calls() {
		let depth = 5;
		let mut listener = Listener::default();

		// Main call
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);

		// Nested calls
		for _ in 0..depth {
			do_evm_call_event(&mut listener);
			do_runtime_step_event(&mut listener);
			do_runtime_step_result_event(&mut listener);
		}

		// Exit all
		for _ in 0..=depth {
			do_exit_event(&mut listener);
		}

		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);

		// Check nested structure
		let mut current = &listener.completed_frames[0];
		for i in 0..depth {
			assert_eq!(current.calls.len(), 1);
			current = &current.calls[0];
			// Each nested call should have incrementing trace address
			assert_eq!(current.trace_address.len(), i + 1);
		}
	}

	#[test]
	fn sibling_calls() {
		let mut listener = Listener::default();

		// Main call
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);

		// First sibling call
		do_evm_call_event(&mut listener);
		do_exit_event(&mut listener);

		// Second sibling call
		do_evm_call_event(&mut listener);
		do_exit_event(&mut listener);

		// Third sibling call
		do_evm_call_event(&mut listener);
		do_exit_event(&mut listener);

		// Main exit
		do_exit_event(&mut listener);

		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		assert_eq!(listener.completed_frames[0].calls.len(), 3);
		assert_eq!(
			listener.completed_frames[0].calls[0].trace_address,
			vec![0]
		);
		assert_eq!(
			listener.completed_frames[0].calls[1].trace_address,
			vec![1]
		);
		assert_eq!(
			listener.completed_frames[0].calls[2].trace_address,
			vec![2]
		);
	}

	// ============ Static Call Tests ============

	#[test]
	fn static_call() {
		let mut listener = Listener::default();
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);

		// Static call
		do_evm_static_call_event(&mut listener);
		do_exit_event(&mut listener);

		// Main exit
		do_exit_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		assert_eq!(listener.completed_frames[0].calls.len(), 1);
		assert_eq!(
			listener.completed_frames[0].calls[0].call_type,
			DebankCallType::StaticCall
		);
	}

	// ============ DelegateCall Tests ============

	#[test]
	fn delegate_call_with_opcode() {
		let mut listener = Listener::default();
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);

		// Set DELEGATECALL opcode
		do_runtime_delegatecall_opcode_event(&mut listener);

		// Nested call (will use DELEGATECALL type)
		do_evm_call_event(&mut listener);
		do_exit_event(&mut listener);

		// Main exit
		do_exit_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		assert_eq!(listener.completed_frames[0].calls.len(), 1);
		assert_eq!(
			listener.completed_frames[0].calls[0].call_type,
			DebankCallType::DelegateCall
		);
	}

	// ============ Suicide Tests ============

	#[test]
	fn call_with_suicide() {
		let mut listener = Listener::default();
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);

		do_evm_suicide_event(&mut listener);

		do_exit_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		assert_eq!(listener.completed_frames[0].calls.len(), 1);
		assert_eq!(
			listener.completed_frames[0].calls[0].call_type,
			DebankCallType::Suicide
		);
		assert!(listener
			.deleted_accounts
			.contains(&H160::from_low_u64_be(100)));
	}

	// ============ Log Tests ============

	#[test]
	fn call_with_log() {
		let mut listener = Listener::default();
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);

		do_evm_log_event(&mut listener);

		do_exit_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		assert_eq!(listener.completed_frames[0].logs.len(), 1);
		assert_eq!(listener.completed_frames[0].logs[0].log_index, 0);
	}

	#[test]
	fn call_with_multiple_logs() {
		let mut listener = Listener::default();
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);

		do_evm_log_event(&mut listener);
		do_evm_log_event(&mut listener);
		do_evm_log_event(&mut listener);

		do_exit_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		assert_eq!(listener.completed_frames[0].logs.len(), 3);
		// Log indices should be sequential
		assert_eq!(listener.completed_frames[0].logs[0].log_index, 0);
		assert_eq!(listener.completed_frames[0].logs[1].log_index, 1);
		assert_eq!(listener.completed_frames[0].logs[2].log_index, 2);
	}

	// ============ Storage Tests ============

	#[test]
	fn call_with_sstore() {
		let mut listener = Listener::default();
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);

		do_runtime_sstore_event(&mut listener);

		do_exit_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		assert!(listener.completed_frames[0].self_storage_change);
		assert!(listener.completed_frames[0].storage_change);
		assert!(listener
			.storage_changes
			.contains_key(&H160::from_low_u64_be(100)));
	}

	#[test]
	fn storage_change_propagates_to_parent() {
		let mut listener = Listener::default();

		// Main call
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);

		// Nested call with sstore
		do_evm_call_event(&mut listener);
		do_runtime_sstore_event(&mut listener);
		do_exit_event(&mut listener);

		// Main exit
		do_exit_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		// Parent should have storage_change but not self_storage_change
		assert!(!listener.completed_frames[0].self_storage_change);
		assert!(listener.completed_frames[0].storage_change);
		// Child should have both
		assert!(listener.completed_frames[0].calls[0].self_storage_change);
		assert!(listener.completed_frames[0].calls[0].storage_change);
	}

	// ============ Precompile Tests ============

	#[test]
	fn precompile_capture_exit_no_subcall() {
		// Precompile that returns via Capture::Exit (no Exit event)
		let mut listener = Listener::default();

		// Main call
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);

		// Precompile subcall (no Exit event will follow for precompile itself)
		do_precompile_subcall_event(&mut listener);
		// Precompile returns via Capture::Exit, continuing parent execution
		// Next event could be LOG or SSTORE from parent
		do_evm_log_event(&mut listener);

		// Main exit
		do_exit_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		// Precompile should be flushed as a child call
		assert_eq!(listener.completed_frames[0].calls.len(), 1);
		assert!(listener.completed_frames[0].calls[0].is_precompile);
		// Log should be on the main call
		assert_eq!(listener.completed_frames[0].logs.len(), 1);
	}

	#[test]
	fn precompile_capture_trap_with_subcall() {
		// Precompile that makes a subcall via Capture::Trap (has Exit event)
		let mut listener = Listener::default();
		let precompile_addr = H160::from_low_u64_be(10);

		// Main call
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);

		// Precompile subcall
		do_precompile_subcall_event_with_caller(
			&mut listener,
			H160::from_low_u64_be(100), // from main call
			precompile_addr,
		);

		// Precompile makes a subcall (Capture::Trap path)
		do_evm_call_event_with_caller(&mut listener, precompile_addr, H160::from_low_u64_be(500));
		// Subcall exits
		do_exit_event(&mut listener);
		// Precompile exits (it gets Exit event because Capture::Trap)
		do_exit_event(&mut listener);

		// Main exit
		do_exit_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		// Main call should have precompile as child
		assert_eq!(listener.completed_frames[0].calls.len(), 1);
		// Precompile should have its subcall as child
		assert_eq!(listener.completed_frames[0].calls[0].calls.len(), 1);
	}

	#[test]
	fn precompile_exit_belongs_to_parent_not_precompile() {
		// Test that Exit event after precompile without subcall goes to parent
		let mut listener = Listener::default();

		// Main call
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);

		// Nested call
		do_evm_call_event(&mut listener);

		// Precompile subcall (Capture::Exit, no Exit event for precompile)
		do_precompile_subcall_event(&mut listener);

		// This Exit should close the nested call, not the precompile
		do_exit_event(&mut listener);

		// Main exit
		do_exit_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		// Main should have one child (the nested call)
		assert_eq!(listener.completed_frames[0].calls.len(), 1);
		// Nested call should have precompile as child
		assert_eq!(listener.completed_frames[0].calls[0].calls.len(), 1);
		assert!(listener.completed_frames[0].calls[0].calls[0].is_precompile);
	}

	// ============ Mixed Call/Create Tests ============

	#[test]
	fn mixed_call_and_create() {
		let mut listener = Listener::default();

		// Main call
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);

		// Nested call
		do_evm_call_event(&mut listener);
		do_exit_event(&mut listener);

		// Nested create
		do_evm_create_event(&mut listener);
		do_exit_event(&mut listener);

		// Main exit
		do_exit_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		assert_eq!(listener.completed_frames[0].calls.len(), 2);
		assert_eq!(
			listener.completed_frames[0].calls[0].call_type,
			DebankCallType::Call
		);
		assert_eq!(
			listener.completed_frames[0].calls[1].call_type,
			DebankCallType::Create
		);
	}

	// ============ parent_failed Flag Tests ============

	#[test]
	fn parent_failed_propagates() {
		let mut listener = Listener::default();

		// Main call
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);

		// Nested call (succeeds)
		do_evm_call_event(&mut listener);
		do_exit_event(&mut listener);

		// Main exits with error
		do_exit_error_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		assert!(listener.completed_frames[0].failed);
		// Child should have parent_failed set
		assert!(listener.completed_frames[0].calls[0].parent_failed);
	}

	// ============ Multiple Transactions Tests ============

	#[test]
	fn multiple_transactions() {
		let mut listener = Listener::default();

		// First transaction
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_exit_event(&mut listener);
		listener.finish_transaction();

		// Second transaction
		do_transact_create_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_exit_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 2);
		assert_eq!(listener.completed_frames[0].call_type, DebankCallType::Call);
		assert_eq!(
			listener.completed_frames[1].call_type,
			DebankCallType::Create
		);
	}

	// ============ pos_in_parent_trace Tests ============

	#[test]
	fn pos_in_parent_trace_with_mixed_calls_and_logs() {
		let mut listener = Listener::default();

		// Main call
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);

		// Log at position 0
		do_evm_log_event(&mut listener);

		// Call at position 1
		do_evm_call_event(&mut listener);
		do_exit_event(&mut listener);

		// Log at position 2
		do_evm_log_event(&mut listener);

		// Call at position 3
		do_evm_call_event(&mut listener);
		do_exit_event(&mut listener);

		// Main exit
		do_exit_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		assert_eq!(listener.completed_frames[0].logs.len(), 2);
		assert_eq!(listener.completed_frames[0].calls.len(), 2);

		// Check positions
		assert_eq!(listener.completed_frames[0].logs[0].position, 0);
		assert_eq!(listener.completed_frames[0].calls[0].pos_in_parent_trace, 1);
		assert_eq!(listener.completed_frames[0].logs[1].position, 2);
		assert_eq!(listener.completed_frames[0].calls[1].pos_in_parent_trace, 3);
	}

	// ============ CallListNew Event Tests ============

	#[test]
	fn call_list_new_event_starts_new_transaction() {
		let mut listener = Listener::default();

		// First transaction via CallListNew
		listener.event(Event::CallListNew());
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_exit_event(&mut listener);

		// Second transaction via CallListNew
		listener.event(Event::CallListNew());
		do_transact_create_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_exit_event(&mut listener);

		// Finish last transaction
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 2);
	}

	// ============ Runtime Capture::Exit Tests ============

	#[test]
	fn runtime_capture_exit_closes_frame() {
		let mut listener = Listener::default();

		// Main call
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);

		// Nested call
		do_evm_call_event(&mut listener);
		// Exit via RuntimeEvent::StepResult with Capture::Exit
		do_runtime_step_result_exit_event(&mut listener);

		// Main exit
		do_exit_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		assert_eq!(listener.completed_frames[0].calls.len(), 1);
	}

	// ============ Double-Exit Prevention Tests ============

	#[test]
	fn no_double_pop_when_step_result_and_evm_exit_both_fire() {
		// In EarlyTransact mode, StepResult(Capture::Exit) + EvmEvent::Exit both
		// fire for the same subcall. Without the flag, this would double-pop.
		let mut listener = Listener::default();

		// Main call
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);

		// Nested call
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);

		// Both StepResult(Exit) and EvmEvent::Exit fire for the nested call
		do_runtime_step_result_exit_event(&mut listener);
		do_exit_event(&mut listener);

		// Main exit
		do_exit_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		assert_eq!(listener.completed_frames[0].calls.len(), 1);
		assert!(!listener.completed_frames[0].failed);
	}

	#[test]
	fn no_double_pop_deeply_nested() {
		// Deeper nesting: without fix, double-pop would pop the parent too.
		let mut listener = Listener::default();

		// Main call
		do_transact_call_event(&mut listener);
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);

		// Level 1 call
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);

		// Level 2 call
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);

		// Both events fire for level 2
		do_runtime_step_result_exit_event(&mut listener);
		do_exit_event(&mut listener);

		// Both events fire for level 1
		do_runtime_step_result_exit_event(&mut listener);
		do_exit_event(&mut listener);

		// Main exit
		do_exit_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		// Level 1 should be child of root
		assert_eq!(listener.completed_frames[0].calls.len(), 1);
		// Level 2 should be child of level 1
		assert_eq!(listener.completed_frames[0].calls[0].calls.len(), 1);
	}

	// ============ Legacy Mode Tests ============

	#[test]
	fn legacy_basic_call() {
		// Legacy mode: no TransactX, no EvmEvent::Exit.
		// Root frame created from EvmEvent::Call, exit from StepResult(Capture::Exit).
		let mut listener = Listener::default();
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_exit_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		assert_eq!(listener.completed_frames[0].call_type, DebankCallType::Call);
	}

	#[test]
	fn legacy_nested_call() {
		let mut listener = Listener::default();
		do_gasometer_event(&mut listener);

		// Root call
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_event(&mut listener);

		// Nested call
		do_evm_call_event(&mut listener);
		do_runtime_step_event(&mut listener);
		do_runtime_step_result_exit_event(&mut listener);

		// Root exit
		do_runtime_step_result_exit_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		assert_eq!(listener.completed_frames[0].calls.len(), 1);
	}

	#[test]
	fn legacy_early_exit_no_runtime() {
		// Legacy mode: call exits before any runtime stepping (e.g. precompile).
		// finish_transaction should handle the leftover frame.
		let mut listener = Listener::default();
		do_gasometer_event(&mut listener);
		do_evm_call_event(&mut listener);
		// No StepResult exit, frame is leftover
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		assert!(listener.completed_frames[0].failed);
		assert!(listener.completed_frames[0]
			.error
			.contains("early exit"));
	}

	#[test]
	fn legacy_record_transaction_only() {
		// Transaction cannot pay for data cost: only RecordTransaction fires, no frames.
		let mut listener = Listener::default();
		do_gasometer_event(&mut listener);
		listener.finish_transaction();

		assert_eq!(listener.completed_frames.len(), 1);
		assert!(listener.completed_frames[0].failed);
		assert!(listener.completed_frames[0]
			.error
			.contains("data cost"));
	}
}
