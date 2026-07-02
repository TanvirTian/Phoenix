//! Control plane (§3.3): UDS server, wire protocol, and VM lifecycle manager.
//!
//! All of this runs on the tokio runtime. It communicates with the synchronous
//! vCPU threads only through the per-VM `std::sync::mpsc` event bridge, never a
//! shared async lock (§1.1).

pub mod manager;
pub mod protocol;
pub mod server;

pub use manager::Manager;
