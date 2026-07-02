#![allow(dead_code)] // scaffold APIs consumed by later phases
//! `vmm-boot` — boot loaders and the guest physical memory map.
//!
//! * [`layout`] — where RAM, MMIO, the framebuffer and PIO devices live.
//! * [`linux`] — the bzImage loader (Phase 2).

pub mod layout;
pub mod linux;

pub use linux::{load_kernel, BootError, BootInfo, E820Entry, FbInfo, KernelImage};
