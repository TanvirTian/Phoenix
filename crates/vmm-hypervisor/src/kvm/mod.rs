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
