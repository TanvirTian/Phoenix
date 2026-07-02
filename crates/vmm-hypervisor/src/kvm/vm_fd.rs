//! `KvmVm`: wraps a `kvm_ioctls::VmFd`, registers guest memory, spawns vCPUs.

use kvm_bindings::{kvm_pit_config, kvm_userspace_memory_region, CpuId};
use kvm_ioctls::VmFd;

use super::vcpu_fd::KvmVcpu;
use crate::traits::{HypervisorError, Vm};

pub struct KvmVm {
    fd: VmFd,
    /// Host-supported CPUID, applied to every vCPU (see `create_vm`).
    cpuid: CpuId,
    /// Whether the host supports pinning the guest TSC frequency.
    tsc_control: bool,
}

impl KvmVm {
    pub(crate) fn new(fd: VmFd, cpuid: CpuId, tsc_control: bool) -> Self {
        Self {
            fd,
            cpuid,
            tsc_control,
        }
    }

    fn ioctl_err(ioctl: &'static str, e: kvm_ioctls::Error) -> HypervisorError {
        HypervisorError::Ioctl {
            ioctl,
            source: std::io::Error::from_raw_os_error(e.errno()),
        }
    }
}

/// Standard addresses for the TSS and identity map (must sit in a hole below
/// the kernel, away from RAM the guest uses; these are the conventional values).
const KVM_TSS_ADDRESS: usize = 0xfffb_d000;
const KVM_IDENTITY_MAP_ADDRESS: u64 = 0xfffb_c000;

impl Vm for KvmVm {
    type Vcpu = KvmVcpu;

    fn arch_setup(&self) -> Result<(), HypervisorError> {
        // Order matters: identity map + TSS before creating the IRQ chip.
        self.fd
            .set_identity_map_address(KVM_IDENTITY_MAP_ADDRESS)
            .map_err(|e| Self::ioctl_err("KVM_SET_IDENTITY_MAP_ADDR", e))?;
        self.fd
            .set_tss_address(KVM_TSS_ADDRESS)
            .map_err(|e| Self::ioctl_err("KVM_SET_TSS_ADDR", e))?;
        // In-kernel IRQ chip (PIC + IOAPIC) so devices can inject interrupts.
        self.fd
            .create_irq_chip()
            .map_err(|e| Self::ioctl_err("KVM_CREATE_IRQCHIP", e))?;
        // Programmable interval timer.
        let pit = kvm_pit_config::default();
        self.fd
            .create_pit2(pit)
            .map_err(|e| Self::ioctl_err("KVM_CREATE_PIT2", e))?;
        Ok(())
    }

    unsafe fn set_user_memory_region(
        &self,
        slot: u32,
        guest_phys_addr: u64,
        size: u64,
        host_addr: u64,
    ) -> Result<(), HypervisorError> {
        let region = kvm_userspace_memory_region {
            slot,
            guest_phys_addr,
            memory_size: size,
            userspace_addr: host_addr,
            flags: 0,
        };
        // SAFETY: caller guarantees `host_addr..host_addr+size` stays mapped for
        // the VM's lifetime (see trait doc).
        self.fd
            .set_user_memory_region(region)
            .map_err(|e| Self::ioctl_err("KVM_SET_USER_MEMORY_REGION", e))
    }

    fn set_irq_line(&self, irq: u32, active: bool) -> Result<(), HypervisorError> {
        self.fd
            .set_irq_line(irq, active)
            .map_err(|e| Self::ioctl_err("KVM_IRQ_LINE", e))
    }

    fn create_vcpu(&self, index: u64) -> Result<Self::Vcpu, HypervisorError> {
        let vcpu_fd = self
            .fd
            .create_vcpu(index)
            .map_err(|e| Self::ioctl_err("KVM_CREATE_VCPU", e))?;
        // Program the guest CPUID before the vCPU ever runs.
        vcpu_fd
            .set_cpuid2(&self.cpuid)
            .map_err(|e| Self::ioctl_err("KVM_SET_CPUID2", e))?;

        // Pin the guest TSC frequency so the guest's "Fast TSC calibration"
        // succeeds instead of failing against the PIT/RTC (which we don't fully
        // emulate) and marking the clock unstable / stalling userspace.
        if self.tsc_control {
            let khz = vcpu_fd.get_tsc_khz().unwrap_or(0);
            let target = if khz > 0 { khz } else { 2_000_000 }; // 2 GHz fallback
            // Best-effort: ignore errors, the guest can still fall back.
            let _ = vcpu_fd.set_tsc_khz(target);
        }

        Ok(KvmVcpu::new(vcpu_fd))
    }
}
