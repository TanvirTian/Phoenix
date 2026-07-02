//! Phase 1 victory condition (§4):
//! "A CLI test runs KVM_RUN once, catches a `VcpuExit::Hlt`, and returns Ok(())."
//!
//! We create a VM with a single 4 KiB RAM page, write a two-byte real-mode stub
//! (`F4` = HLT), point the vCPU at it, run once, and assert we get `Hlt`.
//!
//! println! is intentionally used here (CLI test binary, §1.1). Requires
//! /dev/kvm — in a container without it this prints a clear skip message and
//! exits non-zero, which is expected in CI sandboxes.

use std::process::ExitCode;

use vmm_hypervisor::kvm::memory::GuestRam;
use vmm_hypervisor::traits::{Hypervisor, Vcpu, VcpuExit, Vm};
use vmm_hypervisor::KvmHypervisor;

const GUEST_BASE: u64 = 0x1000;
const GUEST_SIZE: usize = 0x1000; // one page
const MEMSLOT: u32 = 0;

fn run() -> Result<(), Box<dyn std::error::Error>> {
    println!("[hlt-test] opening /dev/kvm ...");
    let hv = KvmHypervisor::new()?;
    println!("[hlt-test] KVM API version = {}", hv.api_version());

    let vm = hv.create_vm()?;

    // Allocate guest RAM and register it with KVM.
    let mem = GuestRam::new_single(GUEST_BASE, GUEST_SIZE, MEMSLOT)?;
    for r in mem.regions() {
        // SAFETY: `mem` (and thus its mmap) outlives the VM in this function.
        unsafe {
            vm.set_user_memory_region(r.slot, r.guest_phys_addr, r.size, r.host_addr)?;
        }
    }

    // Real-mode stub: HLT. (0xF4)
    // A couple of NOPs before it just to show instruction fetch works.
    let code: &[u8] = &[0x90, 0x90, 0xf4]; // nop; nop; hlt
    mem.write_slice(GUEST_BASE, code)?;

    let mut vcpu = vm.create_vcpu(0)?;
    vcpu.setup_initial_state(GUEST_BASE)?;

    println!("[hlt-test] entering KVM_RUN loop ...");
    // Loop is defensive: MMIO/IO before HLT would be unexpected for this stub,
    // but we handle a couple of iterations so a stray exit is reported clearly.
    for _ in 0..8 {
        match vcpu.run()? {
            VcpuExit::Hlt => {
                println!("[hlt-test] caught VcpuExit::Hlt — VICTORY ✔");
                return Ok(());
            }
            VcpuExit::IoOut { port, data } => {
                println!("[hlt-test] (ignored) IoOut port={port:#x} data={data:?}");
            }
            other => {
                return Err(format!("unexpected exit before HLT: {other:?}").into());
            }
        }
    }
    Err("did not reach HLT within iteration budget".into())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[hlt-test] FAILED: {e}");
            eprintln!(
                "[hlt-test] note: this test requires access to /dev/kvm \
                 (hardware virtualization). In a container without it, this \
                 failure is expected."
            );
            ExitCode::FAILURE
        }
    }
}
