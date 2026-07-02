#![allow(dead_code)] // scaffold APIs consumed by later phases
#![allow(clippy::needless_lifetimes)] // explicit lifetimes kept for clarity in device APIs
//! `vmm-devices` — KVM-agnostic device emulation (§3.1).
//!
//! Devices see only byte slices and window-relative offsets; they never touch
//! KVM types. The [`bus::Bus`] dispatches guest MMIO/PIO to registered
//! [`device::Device`]s, and the daemon's exit dispatcher talks only to the Bus.
//!
//! Modules:
//! * [`device`] — the [`Device`](device::Device) trait.
//! * [`bus`] — address-range dispatch ([`Bus`](bus::Bus)).
//! * [`uart`] — 16550 serial (Phase 2).
//! * [`virtio`] — virtqueue + block/net (Phase 4).
//! * [`fb`] — shared-memory framebuffer (Phase 6).

pub mod bus;
pub mod device;
pub mod fb;
pub mod pci_stub;
pub mod rtc_cmos;
pub mod uart;
pub mod virtio;

pub use bus::{Bus, BusRange};
pub use device::Device;
