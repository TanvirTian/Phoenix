//! `vmm-daemon` library surface.
//!
//! The daemon is primarily a binary, but its subsystems are exposed as a
//! library so auxiliary binaries (e.g. the `boot-kernel` Phase 2 test tool) can
//! reuse the exact same boot path, control protocol, and VM types instead of
//! duplicating modules.

// The directory layout is prescribed by the master spec (§2), which nests
// `vm/vm.rs`; several APIs are intentional scaffolding used by later phases.
#![allow(clippy::module_inception)]
#![allow(dead_code)]

pub mod control;
pub mod vm;
