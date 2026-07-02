//! The **Trait Boundary** (§1, Rule 2).
//!
//! The VMM Core talks to the hypervisor *exclusively* through these traits.
//! No KVM types (`kvm_run`, `VcpuExit`, raw fds) ever leak past this module's
//! public surface — everything is expressed with plain Rust values so that the
//! exit dispatcher (`vmm-daemon::vm::exit`) and the device Bus stay
//! KVM-agnostic and unit-testable against a mock.


/// A KVM-independent description of *why* a vCPU stopped running.
///
/// This is deliberately a superset-agnostic, owned representation: the KVM
/// implementation translates `kvm_ioctls::VcpuExit` into this enum and hands it
/// to `handle_exit`. Adding a new backend (e.g. Hypervisor.framework, WHPX)
/// only requires producing these values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VcpuExit {
    /// Guest performed an MMIO *read* at `addr` of `len` bytes. The dispatcher
    /// must produce `len` bytes and complete the exit via
    /// [`Vcpu::set_exit_result`].
    MmioRead { addr: u64, len: usize },
    /// Guest performed an MMIO *write* of `data` to `addr`.
    MmioWrite { addr: u64, data: Vec<u8> },
    /// Guest executed `IN` from `port` for `len` bytes (port I/O read).
    IoIn { port: u16, len: usize },
    /// Guest executed `OUT` of `data` to `port` (port I/O write).
    IoOut { port: u16, data: Vec<u8> },
    /// Guest executed `HLT`.
    Hlt,
    /// Triple fault / guest-requested shutdown.
    Shutdown,
    /// Any exit reason we do not (yet) model. Carries a debug string so the
    /// daemon can log it; the dispatcher turns this into a `DispatchError`.
    Unhandled(String),
}

/// Errors surfaced by the hypervisor abstraction. Typed via `thiserror` (§1.1).
#[derive(thiserror::Error, Debug)]
pub enum HypervisorError {
    #[error("failed to open the hypervisor device: {0}")]
    Open(#[source] std::io::Error),

    #[error("kvm ioctl `{ioctl}` failed: {source}")]
    Ioctl {
        ioctl: &'static str,
        #[source]
        source: std::io::Error,
    },

    #[error("guest memory error: {0}")]
    Memory(String),

    #[error("unsupported KVM API version {found}, expected {expected}")]
    ApiVersion { found: i32, expected: i32 },

    #[error("required KVM capability missing: {0}")]
    MissingCapability(&'static str),

    #[error(
        "attempted to complete a vcpu exit ({attempted}) that does not expect a data payload"
    )]
    ExitCompletionMismatch { attempted: &'static str },
}

/// Top-level entry point: opens `/dev/kvm` (or an equivalent) and creates VMs.
pub trait Hypervisor: Send + Sync {
    type Vm: Vm;

    /// Reported KVM API version. Callers may assert this equals the stable `12`.
    fn api_version(&self) -> i32;

    /// Recommended maximum number of memslots, for sizing the memory map.
    fn max_memslots(&self) -> usize;

    /// Create a fresh VM address space (an anonymous `KVM_CREATE_VM`).
    fn create_vm(&self) -> Result<Self::Vm, HypervisorError>;
}

/// The register/paging state a bzImage needs to be entered in 64-bit long mode.
///
/// The daemon computes these (entry point + zero page from the boot loader,
/// and the GPAs where it wrote the boot GDT and identity page tables) and hands
/// them to [`Vcpu::setup_long_mode`]. Keeping this a plain struct means the
/// long-mode entry contract is expressed without leaking KVM types.
#[derive(Debug, Clone, Copy)]
pub struct LongModeEntry {
    /// 64-bit entry point (RIP).
    pub entry_point: u64,
    /// Guest physical address of boot_params; loaded into RSI.
    pub boot_params: u64,
    /// Top of the initial stack (RSP).
    pub stack_top: u64,
    /// GPA of the PML4 table (loaded into CR3).
    pub pml4_addr: u64,
    /// GPA of the boot GDT.
    pub gdt_addr: u64,
}

/// A single virtual machine: owns guest memory registration and vCPU creation.
pub trait Vm: Send + Sync {
    type Vcpu: Vcpu;

    /// Perform the one-time architectural setup a bootable x86 VM needs:
    /// TSS address, identity-map address, in-kernel IRQ chip, and PIT. Called
    /// once before creating vCPUs.
    fn arch_setup(&self) -> Result<(), HypervisorError>;

    /// Register a slab of host memory as guest physical RAM.
    ///
    /// * `slot` — KVM memory slot index (unique per region).
    /// * `guest_phys_addr` — where the region appears in the guest (GPA).
    /// * `host_addr` — pointer to the backing host virtual memory (HVA).
    /// * `size` — length in bytes.
    ///
    /// # Safety
    /// `host_addr..host_addr+size` must stay mapped and valid for the entire
    /// lifetime of the VM; KVM will read/write it from other threads.
    unsafe fn set_user_memory_region(
        &self,
        slot: u32,
        guest_phys_addr: u64,
        size: u64,
        host_addr: u64,
    ) -> Result<(), HypervisorError>;

    /// Create vCPU number `index` (0-based).
    fn create_vcpu(&self, index: u64) -> Result<Self::Vcpu, HypervisorError>;

    /// Raise or lower a guest IRQ line (level-triggered) on the in-kernel IRQ
    /// chip. Used by device emulation (e.g. the 16550 UART on IRQ 4) to signal
    /// the guest. `active=true` asserts the line, `false` deasserts it.
    fn set_irq_line(&self, irq: u32, active: bool) -> Result<(), HypervisorError>;
}

/// A single virtual CPU. Its `run` loop lives on a dedicated `std::thread`
/// (§1.1) and never `.await`s.
pub trait Vcpu: Send {
    /// Put the vCPU into a sane real-mode/long-mode initial state so it can
    /// begin executing at a known entry point. Backends set segment registers,
    /// RIP/RSP, CR0/CR3/CR4, etc.
    fn setup_initial_state(&mut self, entry_point: u64) -> Result<(), HypervisorError>;

    /// Set the instruction pointer (RIP). Useful for tests / hand-written code.
    fn set_instruction_pointer(&mut self, rip: u64) -> Result<(), HypervisorError>;

    /// Put the vCPU into 64-bit long mode ready to enter a Linux bzImage:
    /// paging on (CR0.PG|PE, CR4.PAE, EFER.LME|LMA), flat 64-bit code/data
    /// segments from `entry.gdt_addr`, `CR3 = entry.pml4_addr`, `RIP =
    /// entry.entry_point`, `RSI = entry.boot_params`, `RSP = entry.stack_top`.
    ///
    /// The daemon must have already written the GDT and page tables into guest
    /// RAM at the GPAs named in `entry`.
    fn setup_long_mode(&mut self, entry: LongModeEntry) -> Result<(), HypervisorError>;

    /// Execute one `KVM_RUN`, returning the abstracted exit reason.
    fn run(&mut self) -> Result<VcpuExit, HypervisorError>;

    /// Debug helper: return a human-readable snapshot of key registers
    /// (RIP, RSP, RFLAGS, CR0/CR3/CR4, EFER, CS). Used to diagnose early boot
    /// faults; not on any hot path.
    fn debug_registers(&self) -> Result<String, HypervisorError>;

    /// Complete a pending *read* exit ([`VcpuExit::MmioRead`] /
    /// [`VcpuExit::IoIn`]) by writing the fetched bytes back into the shared
    /// `kvm_run` structure so the guest sees them on the next `KVM_RUN`.
    ///
    /// Per §3.2's completion contract this is the ONLY place KVM register/exit
    /// state is mutated after an exit — the dispatcher (`exit.rs`) never calls
    /// it, the vCPU *loop* (`vcpu_fd.rs`) does.
    fn set_exit_result(&mut self, data: &[u8]) -> Result<(), HypervisorError>;
}
