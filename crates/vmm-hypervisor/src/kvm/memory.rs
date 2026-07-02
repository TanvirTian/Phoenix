//! Guest memory: a `vm-memory` mmap wrapper plus GPA<->HVA bookkeeping.
//!
//! We keep the backing `GuestMemoryMmap` alive here so the host virtual
//! addresses we hand to `KVM_SET_USER_MEMORY_REGION` remain valid for the whole
//! VM lifetime (see the safety note on `Vm::set_user_memory_region`).

use vm_memory::{
    Bytes, GuestAddress, GuestMemory as _, GuestMemoryMmap, GuestMemoryRegion,
};

use crate::traits::HypervisorError;

/// One contiguous guest RAM region and the host pointer backing it.
#[derive(Debug, Clone, Copy)]
pub struct MemoryRegion {
    pub slot: u32,
    pub guest_phys_addr: u64,
    pub size: u64,
    pub host_addr: u64,
}

/// Owns the mmap'd guest RAM and the list of regions to register with KVM.
pub struct GuestRam {
    inner: GuestMemoryMmap,
    regions: Vec<MemoryRegion>,
}

impl GuestRam {
    /// Allocate a single anonymous, private RAM region of `size` bytes starting
    /// at guest physical address `base_gpa`, assigned to KVM memslot `slot`.
    pub fn new_single(base_gpa: u64, size: usize, slot: u32) -> Result<Self, HypervisorError> {
        let ranges = [(GuestAddress(base_gpa), size)];
        let inner = GuestMemoryMmap::from_ranges(&ranges)
            .map_err(|e| HypervisorError::Memory(format!("mmap guest RAM: {e}")))?;

        let region = inner
            .find_region(GuestAddress(base_gpa))
            .ok_or_else(|| HypervisorError::Memory("region vanished after creation".into()))?;

        // Host virtual address of the start of the region.
        let host_addr = region
            .get_host_address(vm_memory::MemoryRegionAddress(0))
            .map_err(|e| HypervisorError::Memory(format!("host addr: {e}")))? as u64;

        let regions = vec![MemoryRegion {
            slot,
            guest_phys_addr: base_gpa,
            size: size as u64,
            host_addr,
        }];

        Ok(Self { inner, regions })
    }

    /// The regions that must be handed to `KVM_SET_USER_MEMORY_REGION`.
    pub fn regions(&self) -> &[MemoryRegion] {
        &self.regions
    }

    /// Borrow the underlying `vm-memory` object for reads/writes (used by boot
    /// loaders to copy the kernel image into guest RAM).
    pub fn inner(&self) -> &GuestMemoryMmap {
        &self.inner
    }

    /// Total size across all regions, in bytes.
    pub fn total_size(&self) -> u64 {
        self.regions.iter().map(|r| r.size).sum()
    }

    /// Write `bytes` into guest RAM at guest physical address `gpa`.
    pub fn write_slice(&self, gpa: u64, bytes: &[u8]) -> Result<(), HypervisorError> {
        self.inner
            .write_slice(bytes, GuestAddress(gpa))
            .map_err(|e| HypervisorError::Memory(format!("write @ {gpa:#x}: {e}")))
    }

    /// Read `len` bytes from guest RAM at guest physical address `gpa`.
    pub fn read_vec(&self, gpa: u64, len: usize) -> Result<Vec<u8>, HypervisorError> {
        let mut buf = vec![0u8; len];
        self.inner
            .read_slice(&mut buf, GuestAddress(gpa))
            .map_err(|e| HypervisorError::Memory(format!("read @ {gpa:#x}: {e}")))?;
        Ok(buf)
    }

    /// Write a minimal 3-entry boot GDT (null, flat 64-bit code, flat data) to
    /// `gdt_addr`. Selectors: code = 0x08, data = 0x10.
    pub fn write_boot_gdt(&self, gdt_addr: u64) -> Result<(), HypervisorError> {
        // Descriptor encoding helpers.
        let null: u64 = 0;
        // Code: base 0, limit 0xfffff, G=1, L=1 (64-bit), P=1, S=1, type=exec/read.
        let code: u64 = gdt_entry(0xa09b, 0, 0xf_ffff);
        // Data: base 0, limit 0xfffff, G=1, DB=1, P=1, S=1, type=read/write.
        let data: u64 = gdt_entry(0xc093, 0, 0xf_ffff);
        let mut buf = Vec::with_capacity(24);
        buf.extend_from_slice(&null.to_le_bytes());
        buf.extend_from_slice(&code.to_le_bytes());
        buf.extend_from_slice(&data.to_le_bytes());
        self.write_slice(gdt_addr, &buf)
    }

    /// Write identity-mapping page tables covering the first 1 GiB using 2 MiB
    /// pages: one PML4 -> one PDPTE -> one PDE with 512 large-page entries.
    pub fn write_identity_page_tables(
        &self,
        pml4: u64,
        pdpte: u64,
        pde: u64,
    ) -> Result<(), HypervisorError> {
        const PRESENT: u64 = 1 << 0;
        const WRITABLE: u64 = 1 << 1;
        const PAGE_SIZE_2MB: u64 = 1 << 7; // PS bit in PDE

        // PML4[0] -> PDPTE
        self.write_slice(pml4, &(pdpte | PRESENT | WRITABLE).to_le_bytes())?;
        // PDPTE[0] -> PDE
        self.write_slice(pdpte, &(pde | PRESENT | WRITABLE).to_le_bytes())?;
        // PDE[i] -> 2 MiB page at i*2MiB
        let mut buf = Vec::with_capacity(512 * 8);
        for i in 0u64..512 {
            let entry = (i * 0x20_0000) | PRESENT | WRITABLE | PAGE_SIZE_2MB;
            buf.extend_from_slice(&entry.to_le_bytes());
        }
        self.write_slice(pde, &buf)
    }
}

/// Encode a legacy 8-byte GDT descriptor from a 16-bit flags field, base, limit.
/// `flags` packs (in the high word) the access byte and granularity nibble the
/// way `gdt_entry` in the Linux/kvm samples does.
fn gdt_entry(flags: u16, base: u32, limit: u32) -> u64 {
    let mut d: u64 = 0;
    d |= limit as u64 & 0x0000_ffff;
    d |= (base as u64 & 0x00ff_ffff) << 16;
    d |= (flags as u64 & 0x0000_00ff) << 40; // access byte
    d |= ((limit as u64 & 0x000f_0000) >> 16) << 48;
    d |= ((flags as u64 & 0x0000_f000) >> 12) << 52; // granularity nibble
    d |= (base as u64 & 0xff00_0000) << 32;
    d
}
