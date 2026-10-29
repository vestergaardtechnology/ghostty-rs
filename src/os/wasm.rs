//! Port of `src/os/wasm.zig`.
//!
//! STATUS: ported (wasm32-only; not compiled on Linux)
//!
//! This file contains helpers for wasm compilation.

#![cfg(target_arch = "wasm32")]

// @generated-mods-begin (tools/scaffold.py; do not edit by hand)
#[path = "wasm/log.rs"]
pub mod log;
#[path = "wasm/target.rs"]
pub mod target;
// @generated-mods-end

/// True if we're in shared memory mode. If true, then the memory buffer
/// in JS will be backed by a SharedArrayBuffer and some behaviors change.
///
/// TODO(port-dedupe): replace with the build option once build_config is
/// ported (Zig: `options.wasm_shared`, default true).
pub const SHARED_MEM: bool = true;

// Zig: `pub const alloc = std.heap.wasm_allocator`. The return values of
// that allocator should NOT be sent to the host environment unless
// toHostOwned is called on them. In Rust the global allocator is used for
// all allocations; host-owned memory goes through
// `crate::lib::allocator::wasm`.
