//! Guest physical memory map constants (§2).
//!
//! Centralising these keeps the boot loader, device Bus registration, and the
//! daemon in agreement about *where* things live in guest physical space.

/// Base of low RAM (guest physical address 0).
pub const RAM_START: u64 = 0x0000_0000;

/// x86 real-mode boot params ("zero page") load address.
pub const ZERO_PAGE_START: u64 = 0x0000_7000;

/// --- Long-mode boot structures (written by the daemon before entry) --------
/// Boot GDT (null, code, data descriptors) location in guest RAM.
pub const BOOT_GDT_START: u64 = 0x0000_0500;
/// Boot IDT (empty) location.
pub const BOOT_IDT_START: u64 = 0x0000_0520;
/// Identity-mapping page tables (PML4, PDPTE, PDE) for the first 1 GiB.
pub const PML4_START: u64 = 0x0000_9000;
pub const PDPTE_START: u64 = 0x0000_a000;
pub const PDE_START: u64 = 0x0000_b000;

/// Kernel command line load address.
pub const CMDLINE_START: u64 = 0x0002_0000;

/// --- MP tables (Intel MultiProcessor Spec) --------------------------------
/// The guest BIOS scans 0xF0000-0xFFFFF for the MP Floating Pointer ("_MP_").
/// We place the floating pointer here and the configuration table just after.
pub const MPTABLE_START: u64 = 0x000F_0000;

/// Local APIC and IO-APIC MMIO physical addresses (PC standard).
pub const APIC_DEFAULT_PHYS: u32 = 0xFEE0_0000; // local APIC
pub const IOAPIC_DEFAULT_PHYS: u32 = 0xFEC0_0000; // IO-APIC

/// Maximum kernel command line length we support.
pub const CMDLINE_MAX_LEN: usize = 2048;

/// High load address for a 64-bit bzImage protected-mode kernel (`code32_start`
/// default / relocatable load base).
pub const HIGH_RAM_START: u64 = 0x0010_0000; // 1 MiB

/// Where the initrd/initramfs is loaded (kept high, below the MMIO hole).
pub const INITRD_START: u64 = 0x0f00_0000;

/// --- MMIO hole -------------------------------------------------------------
/// virtio-mmio device window base (§2: `VIRTIO_BASE = 0xFE000000`).
pub const VIRTIO_MMIO_BASE: u64 = 0xFE00_0000;
/// Size of each virtio-mmio device's register window.
pub const VIRTIO_MMIO_SIZE: u64 = 0x1000;
/// How many virtio-mmio slots we reserve.
pub const VIRTIO_MMIO_COUNT: u64 = 8;

/// Framebuffer aperture (shared-memory / memfd) base in guest physical space.
pub const FRAMEBUFFER_BASE: u64 = 0xD000_0000;

/// --- Port I/O --------------------------------------------------------------
/// COM1 base port for the 16550 UART.
pub const COM1_PORT_BASE: u16 = 0x3F8;
/// Number of 16550 registers.
pub const COM1_PORT_SIZE: u16 = 8;

/// IRQ line used by COM1 on a PC.
pub const COM1_IRQ: u32 = 4;

/// Guest IRQ base assigned to virtio-mmio devices.
pub const VIRTIO_IRQ_BASE: u32 = 5;

/// Compute the MMIO base of virtio device number `n` (0-based).
pub const fn virtio_mmio_addr(n: u64) -> u64 {
    VIRTIO_MMIO_BASE + n * VIRTIO_MMIO_SIZE
}
