//! Linux KVM implementation of the [`Hypervisor`](crate::traits::Hypervisor)
//! trait family.

pub mod memory;
pub mod vcpu_fd;
pub mod vm_fd;

use kvm_ioctls::Kvm;

use crate::traits::{Hypervisor, HypervisorError};
use vm_fd::KvmVm;

/// The stable KVM API version every modern kernel reports.
pub const KVM_STABLE_API_VERSION: i32 = 12;

/// KVM paravirtualization CPUID leaves. Advertising these makes the guest
/// enable **kvm-clock** — a paravirtual clocksource read from a shared page KVM
/// keeps updated — so the guest never has to calibrate the TSC against the PIT
/// (which fails on a minimal VMM and stalls sub-second timers like `ping`).
const KVM_CPUID_SIGNATURE: u32 = 0x4000_0000;
const KVM_CPUID_FEATURES: u32 = 0x4000_0001;
/// Feature bits (from <asm/kvm_para.h>): old + stable kvmclock MSRs.
const KVM_FEATURE_CLOCKSOURCE: u32 = 1 << 0;
const KVM_FEATURE_CLOCKSOURCE2: u32 = 1 << 3;
const KVM_FEATURE_CLOCKSOURCE_STABLE_BIT: u32 = 1 << 24;

/// Ensure the KVM paravirt signature + feature leaves are present in `cpuid`,
/// so the guest turns on kvm-clock. `get_supported_cpuid` usually includes
/// them, but we add/patch them explicitly to be certain.
fn add_kvm_paravirt_leaves(
    cpuid: kvm_bindings::CpuId,
) -> Result<kvm_bindings::CpuId, HypervisorError> {
    use kvm_bindings::kvm_cpuid_entry2;

    let mut entries: Vec<kvm_cpuid_entry2> = cpuid.as_slice().to_vec();

    // Mask off the TSC-DEADLINE LAPIC-timer feature (CPUID leaf 1, ECX bit 24).
    // TSC-deadline is a *one-shot* timer: the guest must rewrite the deadline
    // MSR after every tick, and on a minimal VMM with an unstable/uncalibrated
    // TSC that re-arm cycle breaks — the LAPIC timer fires a few times during
    // boot then stops, so periodic scheduler ticks / process wakeups die (this
    // is what stalls `ping` after the first packet). Clearing the bit forces
    // the guest to use the classic periodic LAPIC timer, which KVM's in-kernel
    // APIC drives reliably.
    const TSC_DEADLINE_BIT: u32 = 1 << 24;
    for e in entries.iter_mut() {
        if e.function == 1 && e.index == 0 {
            e.ecx &= !TSC_DEADLINE_BIT;
        }
    }

    // Drop any existing paravirt entries so we can set them cleanly.
    entries.retain(|e| e.function != KVM_CPUID_SIGNATURE && e.function != KVM_CPUID_FEATURES);

    // 0x40000000: signature. eax = max paravirt leaf; ebx/ecx/edx = "KVMKVMKVM\0\0\0".
    entries.push(kvm_cpuid_entry2 {
        function: KVM_CPUID_SIGNATURE,
        index: 0,
        flags: 0,
        eax: KVM_CPUID_FEATURES, // highest supported paravirt leaf
        ebx: u32::from_le_bytes(*b"KVMK"),
        ecx: u32::from_le_bytes(*b"VMKV"),
        edx: u32::from_le_bytes(*b"M\0\0\0"),
        padding: [0; 3],
    });
    // 0x40000001: feature bits. eax = the kvmclock features we support.
    entries.push(kvm_cpuid_entry2 {
        function: KVM_CPUID_FEATURES,
        index: 0,
        flags: 0,
        eax: KVM_FEATURE_CLOCKSOURCE
            | KVM_FEATURE_CLOCKSOURCE2
            | KVM_FEATURE_CLOCKSOURCE_STABLE_BIT,
        ebx: 0,
        ecx: 0,
        edx: 0,
        padding: [0; 3],
    });

    kvm_bindings::CpuId::from_entries(&entries).map_err(|_| {
        HypervisorError::Memory("failed to rebuild CPUID with kvm paravirt leaves".into())
    })
}

/// Owns the `/dev/kvm` handle.
pub struct KvmHypervisor {
    kvm: Kvm,
}

impl KvmHypervisor {
    /// Open `/dev/kvm` and sanity-check the API version.
    pub fn new() -> Result<Self, HypervisorError> {
        let kvm = Kvm::new().map_err(|e| {
            HypervisorError::Open(std::io::Error::from_raw_os_error(e.errno()))
        })?;

        let version = kvm.get_api_version();
        if version != KVM_STABLE_API_VERSION {
            return Err(HypervisorError::ApiVersion {
                found: version,
                expected: KVM_STABLE_API_VERSION,
            });
        }
        Ok(Self { kvm })
    }
}

impl Hypervisor for KvmHypervisor {
    type Vm = KvmVm;

    fn api_version(&self) -> i32 {
        self.kvm.get_api_version()
    }

    fn max_memslots(&self) -> usize {
        self.kvm.get_nr_memslots()
    }

    fn create_vm(&self) -> Result<Self::Vm, HypervisorError> {
        let vm_fd = self.kvm.create_vm().map_err(|e| HypervisorError::Ioctl {
            ioctl: "KVM_CREATE_VM",
            source: std::io::Error::from_raw_os_error(e.errno()),
        })?;
        // Fetch the host-supported CPUID once; each vCPU is programmed with it
        // via KVM_SET_CPUID2 so the guest kernel's early feature probing works.
        // Without this, modern x86-64 kernels fault immediately on boot.
        let cpuid = self
            .kvm
            .get_supported_cpuid(kvm_bindings::KVM_MAX_CPUID_ENTRIES)
            .map_err(|e| HypervisorError::Ioctl {
                ioctl: "KVM_GET_SUPPORTED_CPUID",
                source: std::io::Error::from_raw_os_error(e.errno()),
            })?;
        // Advertise the KVM paravirt leaves so the guest enables kvm-clock.
        let cpuid = add_kvm_paravirt_leaves(cpuid)?;
        // Whether we can pin the guest TSC frequency (avoids the guest failing
        // TSC calibration against the PIT/RTC and marking the clock unstable).
        let tsc_khz = if self.kvm.check_extension(kvm_ioctls::Cap::GetTscKhz)
            && self.kvm.check_extension(kvm_ioctls::Cap::TscControl)
        {
            // Ask the host for a sane default via a throwaway vcpu? Simpler: use
            // the KVM system-wide value if exposed; else a fixed 2 GHz. We read
            // it from the first created vcpu in create_vcpu instead.
            Some(())
        } else {
            None
        };
        Ok(KvmVm::new(vm_fd, cpuid, tsc_khz.is_some()))
    }
}
