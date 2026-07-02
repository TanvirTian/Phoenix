#![allow(dead_code)] // scaffold APIs consumed by later phases
//! `vmm-hypervisor` — the KVM abstraction layer (the **Trait Boundary**, §1).
//!
//! Public surface:
//! * [`traits`] — [`Hypervisor`](traits::Hypervisor), [`Vm`](traits::Vm),
//!   [`Vcpu`](traits::Vcpu) and the KVM-agnostic [`VcpuExit`](traits::VcpuExit).
//! * [`kvm`] — the concrete Linux KVM backend implementing those traits.
//!
//! Everything above this crate (device Bus, exit dispatcher, daemon) depends
//! only on the traits, never on `kvm` directly, so devices stay testable in
//! isolation and a second backend can be dropped in later.

pub mod kvm;
pub mod traits;

pub use kvm::KvmHypervisor;
pub use traits::{Hypervisor, HypervisorError, Vcpu, VcpuExit, Vm};
