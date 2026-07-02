//! The vCPU wrapper — where the `KVM_RUN` loop lives (§2).
//!
//! Implements the §3.2 completion contract: `run()` translates the raw KVM exit
//! into an abstract [`VcpuExit`], remembering enough context (the pending read
//! kind) so that a later [`set_exit_result`](Vcpu::set_exit_result) can write
//! the fetched bytes back into `kvm_run` before the next `KVM_RUN`.

use kvm_bindings::{kvm_fpu, kvm_regs, kvm_segment, kvm_sregs};
use kvm_ioctls::{VcpuExit as KvmExit, VcpuFd};

use crate::traits::{HypervisorError, LongModeEntry, Vcpu, VcpuExit};

// Control-register bits.
const CR0_PE: u64 = 1 << 0; // protected mode enable
const CR0_MP: u64 = 1 << 1;
const CR0_ET: u64 = 1 << 4;
const CR0_PG: u64 = 1 << 31; // paging
const CR4_PAE: u64 = 1 << 5; // physical address extension
// EFER bits.
const EFER_LME: u64 = 1 << 8; // long mode enable
const EFER_LMA: u64 = 1 << 10; // long mode active

/// Build a flat 64-bit code segment descriptor mirror (for KVM sregs).
fn code_segment(selector: u16) -> kvm_segment {
    kvm_segment {
        base: 0,
        limit: 0xffff_ffff,
        selector,
        type_: 0b1011, // execute/read, accessed
        present: 1,
        dpl: 0,
        db: 0,
        s: 1, // code/data
        l: 1, // 64-bit
        g: 1, // 4 KiB granularity
        avl: 0,
        unusable: 0,
        padding: 0,
    }
}

/// Build a flat data segment descriptor mirror.
fn data_segment(selector: u16) -> kvm_segment {
    kvm_segment {
        base: 0,
        limit: 0xffff_ffff,
        selector,
        type_: 0b0011, // read/write, accessed
        present: 1,
        dpl: 0,
        db: 1,
        s: 1,
        l: 0,
        g: 1,
        avl: 0,
        unusable: 0,
        padding: 0,
    }
}

/// Which kind of read exit is awaiting completion. We stash the address/port so
/// `set_exit_result` can route the bytes to the right KVM helper.
#[derive(Debug, Clone, Copy)]
enum PendingRead {
    None,
    Mmio { addr: u64, len: usize },
    Io { port: u16, len: usize },
}

pub struct KvmVcpu {
    fd: VcpuFd,
    pending: PendingRead,
}

impl KvmVcpu {
    pub(crate) fn new(fd: VcpuFd) -> Self {
        Self {
            fd,
            pending: PendingRead::None,
        }
    }

    fn ioctl_err(ioctl: &'static str, e: kvm_ioctls::Error) -> HypervisorError {
        HypervisorError::Ioctl {
            ioctl,
            source: std::io::Error::from_raw_os_error(e.errno()),
        }
    }
}

impl Vcpu for KvmVcpu {
    fn setup_initial_state(&mut self, entry_point: u64) -> Result<(), HypervisorError> {
        // --- Special registers: flat 64-bit-friendly real-mode-ish setup ---
        // For a bzImage 64-bit protocol we generally enter in long mode, but a
        // minimal, backend-neutral default is to start in real mode with CS
        // based at the entry point. The Linux boot loader (vmm-boot) overrides
        // segment/paging state as needed. Here we provide a sane baseline so a
        // hand-written HLT stub can execute (Phase 1 victory condition).
        let mut sregs: kvm_sregs = self
            .fd
            .get_sregs()
            .map_err(|e| Self::ioctl_err("KVM_GET_SREGS", e))?;

        // Real mode: CS:IP. Put the code segment base at the page containing the
        // entry point so RIP can be a small offset.
        let cs_base = entry_point & !0xffff;
        sregs.cs.base = cs_base;
        sregs.cs.selector = (cs_base >> 4) as u16;

        self.fd
            .set_sregs(&sregs)
            .map_err(|e| Self::ioctl_err("KVM_SET_SREGS", e))?;

        let mut regs: kvm_regs = self
            .fd
            .get_regs()
            .map_err(|e| Self::ioctl_err("KVM_GET_REGS", e))?;
        regs.rip = entry_point - cs_base;
        regs.rflags = 0x2; // reserved bit, interrupts disabled
        self.fd
            .set_regs(&regs)
            .map_err(|e| Self::ioctl_err("KVM_SET_REGS", e))?;

        Ok(())
    }

    fn set_instruction_pointer(&mut self, rip: u64) -> Result<(), HypervisorError> {
        let mut regs = self
            .fd
            .get_regs()
            .map_err(|e| Self::ioctl_err("KVM_GET_REGS", e))?;
        regs.rip = rip;
        self.fd
            .set_regs(&regs)
            .map_err(|e| Self::ioctl_err("KVM_SET_REGS", e))
    }

    fn setup_long_mode(&mut self, entry: LongModeEntry) -> Result<(), HypervisorError> {
        let mut sregs: kvm_sregs = self
            .fd
            .get_sregs()
            .map_err(|e| Self::ioctl_err("KVM_GET_SREGS", e))?;

        // Flat long-mode segments. Selectors match the boot GDT the daemon
        // wrote (index 1 = code = 0x08, index 2 = data = 0x10).
        let code = code_segment(0x08);
        let data = data_segment(0x10);
        sregs.cs = code;
        sregs.ds = data;
        sregs.es = data;
        sregs.fs = data;
        sregs.gs = data;
        sregs.ss = data;

        // GDT register points at the boot GDT (3 entries * 8 bytes).
        sregs.gdt.base = entry.gdt_addr;
        sregs.gdt.limit = (3 * 8 - 1) as u16;

        // Paging: CR3 -> PML4, CR4.PAE, CR0.PG|PE, EFER.LME|LMA.
        sregs.cr3 = entry.pml4_addr;
        sregs.cr4 |= CR4_PAE;
        sregs.cr0 |= CR0_PE | CR0_MP | CR0_ET | CR0_PG;
        sregs.efer |= EFER_LME | EFER_LMA;

        self.fd
            .set_sregs(&sregs)
            .map_err(|e| Self::ioctl_err("KVM_SET_SREGS", e))?;

        // General registers: RIP=entry, RSI=boot_params, RSP=stack_top.
        let mut regs: kvm_regs = self
            .fd
            .get_regs()
            .map_err(|e| Self::ioctl_err("KVM_GET_REGS", e))?;
        regs.rip = entry.entry_point;
        regs.rsi = entry.boot_params;
        regs.rsp = entry.stack_top;
        regs.rbp = entry.stack_top;
        regs.rflags = 0x2; // reserved bit set, IF clear
        self.fd
            .set_regs(&regs)
            .map_err(|e| Self::ioctl_err("KVM_SET_REGS", e))?;

        // Initialize a clean FPU/SSE state. Modern kernels use SSE/AVX string
        // and memcpy routines very early; without a valid MXCSR / x87 control
        // word those instructions fault and the guest wedges in a tight loop.
        // fcw=0x37f is the x87 default control word; mxcsr=0x1f80 the SSE default.
        let fpu = kvm_fpu {
            fcw: 0x37f,
            mxcsr: 0x1f80,
            ..Default::default()
        };
        self.fd
            .set_fpu(&fpu)
            .map_err(|e| Self::ioctl_err("KVM_SET_FPU", e))?;

        Ok(())
    }

    fn debug_registers(&self) -> Result<String, HypervisorError> {
        let regs = self
            .fd
            .get_regs()
            .map_err(|e| Self::ioctl_err("KVM_GET_REGS", e))?;
        let sregs = self
            .fd
            .get_sregs()
            .map_err(|e| Self::ioctl_err("KVM_GET_SREGS", e))?;
        Ok(format!(
            "RIP={:#018x} RSP={:#018x} RFLAGS={:#x} RSI={:#x}\n\
             CR0={:#x} CR3={:#x} CR4={:#x} EFER={:#x}\n\
             CS.base={:#x} CS.sel={:#x} CS.l={} CS.db={} CS.present={}\n\
             GDT.base={:#x} GDT.limit={:#x}",
            regs.rip, regs.rsp, regs.rflags, regs.rsi,
            sregs.cr0, sregs.cr3, sregs.cr4, sregs.efer,
            sregs.cs.base, sregs.cs.selector, sregs.cs.l, sregs.cs.db, sregs.cs.present,
            sregs.gdt.base, sregs.gdt.limit,
        ))
    }

    fn run(&mut self) -> Result<VcpuExit, HypervisorError> {
        // Clear any stale pending-read state; only a fresh read exit sets it.
        self.pending = PendingRead::None;

        let exit = self
            .fd
            .run()
            .map_err(|e| Self::ioctl_err("KVM_RUN", e))?;

        let translated = match exit {
            KvmExit::MmioRead(addr, data) => {
                self.pending = PendingRead::Mmio {
                    addr,
                    len: data.len(),
                };
                VcpuExit::MmioRead {
                    addr,
                    len: data.len(),
                }
            }
            KvmExit::MmioWrite(addr, data) => VcpuExit::MmioWrite {
                addr,
                data: data.to_vec(),
            },
            KvmExit::IoIn(port, data) => {
                self.pending = PendingRead::Io {
                    port,
                    len: data.len(),
                };
                VcpuExit::IoIn {
                    port,
                    len: data.len(),
                }
            }
            KvmExit::IoOut(port, data) => VcpuExit::IoOut {
                port,
                data: data.to_vec(),
            },
            KvmExit::Hlt => VcpuExit::Hlt,
            KvmExit::Shutdown => VcpuExit::Shutdown,
            other => VcpuExit::Unhandled(format!("{other:?}")),
        };

        Ok(translated)
    }

    fn set_exit_result(&mut self, data: &[u8]) -> Result<(), HypervisorError> {
        // The kvm_ioctls crate exposes the exit payload buffer through the
        // `VcpuExit::MmioRead(_, buf)` / `IoIn(_, buf)` slices returned by
        // `run()`. To write the completion we take a fresh mutable view of
        // `kvm_run` and copy the bytes into the data region, matching the
        // address/port we recorded in `pending`.
        match self.pending {
            PendingRead::Mmio { addr, len } => {
                if data.len() != len {
                    return Err(HypervisorError::Memory(format!(
                        "mmio completion length mismatch: expected {len}, got {}",
                        data.len()
                    )));
                }
                // SAFETY: we only touch the mmio.data union field, bounded by
                // `len`, on the current thread between two KVM_RUNs.
                let run = self.fd.get_kvm_run();
                // kvm_run is a union; access the mmio member.
                let mmio = unsafe { &mut run.__bindgen_anon_1.mmio };
                debug_assert_eq!(mmio.phys_addr, addr);
                let _ = addr;
                mmio.data[..len].copy_from_slice(data);
                Ok(())
            }
            PendingRead::Io { port, len } => {
                if data.len() != len {
                    return Err(HypervisorError::Memory(format!(
                        "io completion length mismatch: expected {len}, got {}",
                        data.len()
                    )));
                }
                let run = self.fd.get_kvm_run();
                // SAFETY: single-threaded access to the io payload between runs.
                let io = unsafe { run.__bindgen_anon_1.io };
                let offset = io.data_offset as usize;
                let _ = port;
                // The io data lives at `data_offset` bytes into the kvm_run page.
                let base = run as *mut kvm_bindings::kvm_run as *mut u8;
                // SAFETY: offset/len come from KVM and fit within the shared run page.
                unsafe {
                    let dst = base.add(offset);
                    std::ptr::copy_nonoverlapping(data.as_ptr(), dst, len);
                }
                Ok(())
            }
            PendingRead::None => Err(HypervisorError::ExitCompletionMismatch {
                attempted: "no pending read exit",
            }),
        }
    }
}
