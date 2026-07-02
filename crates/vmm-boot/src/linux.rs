//! Linux `bzImage` loader (Phase 2, §4) — 64-bit boot protocol.
//!
//! Parses the x86 setup header, copies the protected-mode kernel to high RAM,
//! writes the command line, and builds the boot params ("zero page") including
//! an e820 memory map. The daemon then enters the vCPU in **long mode** at
//! `entry_point` with `RSI = zero_page` (see `vmm-hypervisor` long-mode setup).
//!
//! We address boot_params / setup_header by documented byte offsets (they are
//! Linux boot-protocol structs, not KVM structs) to avoid an extra dependency.

use crate::layout;

// --- setup_header field offsets (within the bzImage, absolute) --------------
const OFF_SETUP_SECTS: usize = 0x1f1; // u8
const OFF_HDR_MAGIC: usize = 0x202; // "HdrS"
const OFF_VERSION: usize = 0x206; // u16
const OFF_TYPE_OF_LOADER: usize = 0x210; // u8
const OFF_LOADFLAGS: usize = 0x211; // u8
const OFF_RAMDISK_IMAGE: usize = 0x218; // u32
const OFF_RAMDISK_SIZE: usize = 0x21c; // u32
const OFF_CMD_LINE_PTR: usize = 0x228; // u32
const OFF_KERNEL_ALIGNMENT: usize = 0x230; // u32
const OFF_XLOADFLAGS: usize = 0x236; // u16

const HDR_MAGIC: &[u8; 4] = b"HdrS";

// loadflags bits.
const LOADED_HIGH: u8 = 0x01; // kernel is loaded high (>=1MiB)
const CAN_USE_HEAP: u8 = 0x80;

// --- boot_params ("zero page") field offsets --------------------------------
const BP_E820_ENTRIES: usize = 0x1e8; // u8: number of e820 entries
const BP_E820_TABLE: usize = 0x2d0; // start of e820 entries (20 bytes each)
const BP_HDR_OFFSET: usize = 0x1f1; // setup_header lives at 0x1f1..0x268 in bp
const E820_ENTRY_SIZE: usize = 20;
const E820_RAM: u32 = 1;
const E820_MAX_ENTRIES: usize = 128;

#[derive(thiserror::Error, Debug)]
pub enum BootError {
    #[error("kernel image too small ({0} bytes) to contain a boot header")]
    TooSmall(usize),
    #[error("not a bzImage: missing 'HdrS' magic at {OFF_HDR_MAGIC:#x}")]
    BadMagic,
    #[error("boot protocol {major}.{minor} too old; need >= 2.06 for 32/64-bit entry")]
    ProtocolTooOld { major: u8, minor: u8 },
    #[error("kernel not loaded-high capable (loadflags LOADED_HIGH clear)")]
    NotRelocatableHigh,
    #[error("command line too long: {0} > {max}", max = layout::CMDLINE_MAX_LEN)]
    CmdlineTooLong(usize),
    #[error("guest memory error: {0}")]
    Memory(String),
}

/// Everything the daemon needs to point a vCPU at the loaded kernel.
#[derive(Debug, Clone)]
pub struct BootInfo {
    /// Guest physical 64-bit entry point.
    pub entry_point: u64,
    /// GPA of the boot params ("zero page"); goes in RSI.
    pub zero_page: u64,
    /// GPA of the command line.
    pub cmdline_addr: u64,
}

/// An e820 region to advertise to the guest.
#[derive(Debug, Clone, Copy)]
pub struct E820Entry {
    pub addr: u64,
    pub size: u64,
    pub kind: u32,
}

impl E820Entry {
    pub fn ram(addr: u64, size: u64) -> Self {
        Self {
            addr,
            size,
            kind: E820_RAM,
        }
    }
}

/// Parsed view over a bzImage.
pub struct KernelImage<'a> {
    image: &'a [u8],
    pm_kernel_offset: usize,
    version: u16,
    loadflags: u8,
}

impl<'a> KernelImage<'a> {
    pub fn parse(image: &'a [u8]) -> Result<Self, BootError> {
        if image.len() < 0x1000 {
            return Err(BootError::TooSmall(image.len()));
        }
        if &image[OFF_HDR_MAGIC..OFF_HDR_MAGIC + 4] != HDR_MAGIC {
            return Err(BootError::BadMagic);
        }
        let version = u16::from_le_bytes([image[OFF_VERSION], image[OFF_VERSION + 1]]);
        let major = (version >> 8) as u8;
        let minor = (version & 0xff) as u8;
        if version < 0x0206 {
            return Err(BootError::ProtocolTooOld { major, minor });
        }
        let loadflags = image[OFF_LOADFLAGS];
        if loadflags & LOADED_HIGH == 0 {
            return Err(BootError::NotRelocatableHigh);
        }

        let mut setup_sects = image[OFF_SETUP_SECTS] as usize;
        if setup_sects == 0 {
            setup_sects = 4;
        }
        let pm_kernel_offset = (setup_sects + 1) * 512;

        Ok(Self {
            image,
            pm_kernel_offset,
            version,
            loadflags,
        })
    }

    pub fn protected_mode_kernel(&self) -> &[u8] {
        &self.image[self.pm_kernel_offset..]
    }

    /// The setup_header bytes (from 0x1f1 up to 0x268) to copy into the zero
    /// page verbatim before patching.
    fn setup_header_bytes(&self) -> &[u8] {
        // Header spans 0x1f1..0x268 in the boot sector.
        &self.image[BP_HDR_OFFSET..0x268]
    }

    pub fn version(&self) -> u16 {
        self.version
    }
}

/// Linear-framebuffer info for the guest's `screen_info` (Phase 6). Lets Linux
/// create `/dev/fb0` via the generic system framebuffer without a device tree.
#[derive(Debug, Clone, Copy)]
pub struct FbInfo {
    pub base: u64,
    pub width: u32,
    pub height: u32,
    pub bpp: u32,
}

/// Load `image` into guest RAM and build the zero page.
///
/// * `write(gpa, bytes)` copies into guest RAM (injected so this crate stays
///   independent of the concrete memory type).
/// * `e820` is the physical memory map to advertise (typically one RAM entry
///   for low memory below the MMIO hole).
/// * `fb` (optional) fills `screen_info` so the guest gets `/dev/fb0`.
#[allow(clippy::too_many_arguments)]
pub fn load_kernel<W>(
    image: &[u8],
    cmdline: &str,
    e820: &[E820Entry],
    initrd: Option<&[u8]>,
    fb: Option<FbInfo>,
    write: W,
) -> Result<BootInfo, BootError>
where
    W: FnMut(u64, &[u8]) -> Result<(), String>,
{
    let mut write = write;
    let kernel = KernelImage::parse(image)?;

    if cmdline.len() >= layout::CMDLINE_MAX_LEN {
        return Err(BootError::CmdlineTooLong(cmdline.len()));
    }

    // 1. Protected-mode kernel -> high RAM (1 MiB).
    write(layout::HIGH_RAM_START, kernel.protected_mode_kernel())
        .map_err(BootError::Memory)?;

    // 2. Command line (NUL-terminated).
    let mut cmd = cmdline.as_bytes().to_vec();
    cmd.push(0);
    write(layout::CMDLINE_START, &cmd).map_err(BootError::Memory)?;

    // 3. Build the zero page in a local 4 KiB buffer, then write it once.
    let mut zp = vec![0u8; 0x1000];

    // 3a. Copy the setup_header into the zero page at the same offset (0x1f1).
    let hdr = kernel.setup_header_bytes();
    let hdr_end = BP_HDR_OFFSET + hdr.len();
    zp[BP_HDR_OFFSET..hdr_end].copy_from_slice(hdr);

    // 3b. Patch header fields for our loader.
    zp[OFF_TYPE_OF_LOADER] = 0xff; // undefined/other bootloader
    zp[OFF_LOADFLAGS] = kernel.loadflags | CAN_USE_HEAP;
    put_u32(&mut zp, OFF_CMD_LINE_PTR, layout::CMDLINE_START as u32);

    // Load the initrd (if any) into high guest RAM and record its location so
    // the kernel can find and unpack it as the initramfs.
    if let Some(rd) = initrd {
        write(layout::INITRD_START, rd).map_err(BootError::Memory)?;
        put_u32(&mut zp, OFF_RAMDISK_IMAGE, layout::INITRD_START as u32);
        put_u32(&mut zp, OFF_RAMDISK_SIZE, rd.len() as u32);
        if debug_enabled() {
            eprintln!(
                "[boot] initrd loaded at {:#x} ({} bytes)",
                layout::INITRD_START,
                rd.len()
            );
        }
    }

    // Ensure the sentinel (0x1ef) is zero so the kernel TRUSTS the boot_params
    // fields we filled (e820_entries, ramdisk, etc.). If left 0xff the kernel
    // wipes those fields and falls back to a tiny default memory map, causing
    // "alloc_low_pages: can not alloc memory". We zeroed the whole buffer, but
    // assert it explicitly since it's load-bearing.
    zp[0x1ef] = 0x00; // sentinel = clean

    // Belt-and-suspenders legacy memory fields (KB above 1 MiB), in case the
    // kernel's e820 path is bypassed. total_high_kb = (usable - 1MiB) / 1024.
    let total_kb_above_1m: u64 = {
        let total: u64 = e820
            .iter()
            .filter(|e| e.kind == E820_RAM)
            .map(|e| e.addr + e.size)
            .max()
            .unwrap_or(0);
        total.saturating_sub(0x10_0000) / 1024
    };
    // ext_mem_k @ 0x1e0 (also alt_mem_k). Cap ext_mem_k at ~64MiB-1 (u16-ish
    // legacy) is NOT needed here since it's a u32; write the full value.
    put_u32(&mut zp, 0x1e0, total_kb_above_1m.min(u32::MAX as u64) as u32);
    // (ramdisk fields are set above when an initrd is provided; otherwise the
    // zeroed buffer already leaves them 0 = no initrd.)
    let _ = OFF_KERNEL_ALIGNMENT;
    let _ = OFF_XLOADFLAGS;

    // 3c. e820 memory map.
    let n = e820.len().min(E820_MAX_ENTRIES);
    zp[BP_E820_ENTRIES] = n as u8;
    for (i, e) in e820.iter().take(n).enumerate() {
        let base = BP_E820_TABLE + i * E820_ENTRY_SIZE;
        put_u64(&mut zp, base, e.addr);
        put_u64(&mut zp, base + 8, e.size);
        put_u32(&mut zp, base + 16, e.kind);
        if debug_enabled() {
            eprintln!(
                "[boot] e820[{i}] addr={:#x} size={:#x} ({} MiB) kind={}",
                e.addr,
                e.size,
                e.size / (1024 * 1024),
                e.kind
            );
        }
    }

    // 3d. screen_info (boot_params offset 0) for a linear framebuffer, so the
    // guest's generic system framebuffer (simplefb/sysfb) creates /dev/fb0.
    // On x86 with no device tree, this "VESA VLFB" handoff is how firmware/boot
    // loaders normally advertise a framebuffer.
    if let Some(fb) = fb {
        write_screen_info(&mut zp, fb);
    }

    write(layout::ZERO_PAGE_START, &zp).map_err(BootError::Memory)?;

    // 64-bit entry point = protected-mode load address + 0x200.
    Ok(BootInfo {
        entry_point: layout::HIGH_RAM_START + 0x200,
        zero_page: layout::ZERO_PAGE_START,
        cmdline_addr: layout::CMDLINE_START,
    })
}

/// Debug output gated behind the `VMM_DEBUG` env var (keeps normal boots quiet).
fn debug_enabled() -> bool {
    std::env::var_os("VMM_DEBUG").is_some()
}

fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}
fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}

/// Fill `screen_info` (boot_params offset 0x00) with a linear framebuffer so the
/// guest kernel creates `/dev/fb0`. Field offsets per `screen_info.h`.
fn write_screen_info(zp: &mut [u8], fb: FbInfo) {
    // orig_video_isVGA @ 0x0f = VIDEO_TYPE_VLFB (0x23): VESA linear framebuffer.
    // This is picked up by the generic system framebuffer (sysfb -> simplefb)
    // on x86 without needing EFI or a device tree.
    zp[0x0f] = 0x23; // VIDEO_TYPE_VLFB
    put_u16(zp, 0x12, fb.width as u16); // lfb_width
    put_u16(zp, 0x14, fb.height as u16); // lfb_height
    put_u16(zp, 0x16, fb.bpp as u16); // lfb_depth
    put_u32(zp, 0x18, fb.base as u32); // lfb_base (low 32 bits)
    let line = fb.width * (fb.bpp / 8);
    let size = line * fb.height;
    put_u32(zp, 0x1c, size); // lfb_size (bytes)
    put_u16(zp, 0x24, line as u16); // lfb_linelength (stride)

    // XRGB8888 channel layout: B[7:0] G[15:8] R[23:16] X[31:24].
    // red
    zp[0x26] = 8; // red_size
    zp[0x27] = 16; // red_pos
    // green
    zp[0x28] = 8; // green_size
    zp[0x29] = 8; // green_pos
    // blue
    zp[0x2a] = 8; // blue_size
    zp[0x2b] = 0; // blue_pos
    // reserved / alpha
    zp[0x2c] = 8; // rsvd_size
    zp[0x2d] = 24; // rsvd_pos

    // capabilities @ 0x36; ext_lfb_base @ 0x3a (0 since base fits in 32 bits).
    put_u32(zp, 0x36, 0);
    put_u32(zp, 0x3a, 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a fake but structurally-valid bzImage header for parser tests.
    fn fake_bzimage() -> Vec<u8> {
        let mut img = vec![0u8; 0x2000];
        img[OFF_SETUP_SECTS] = 1; // -> pm offset = (1+1)*512 = 1024
        img[OFF_HDR_MAGIC..OFF_HDR_MAGIC + 4].copy_from_slice(HDR_MAGIC);
        img[OFF_VERSION] = 0x0c; // 2.12 little-endian: 0x020c
        img[OFF_VERSION + 1] = 0x02;
        img[OFF_LOADFLAGS] = LOADED_HIGH;
        img
    }

    #[test]
    fn parses_valid_header() {
        let img = fake_bzimage();
        let k = KernelImage::parse(&img).unwrap();
        assert_eq!(k.version(), 0x020c);
        assert_eq!(k.protected_mode_kernel().len(), img.len() - 1024);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut img = fake_bzimage();
        img[OFF_HDR_MAGIC] = 0;
        assert!(matches!(KernelImage::parse(&img), Err(BootError::BadMagic)));
    }

    #[test]
    fn rejects_old_protocol() {
        let mut img = fake_bzimage();
        img[OFF_VERSION] = 0x05;
        img[OFF_VERSION + 1] = 0x02; // 2.05
        assert!(matches!(
            KernelImage::parse(&img),
            Err(BootError::ProtocolTooOld { .. })
        ));
    }

    #[test]
    fn load_writes_zero_page_with_e820_and_entry() {
        use std::cell::RefCell;
        let img = fake_bzimage();
        let writes: RefCell<Vec<(u64, Vec<u8>)>> = RefCell::new(Vec::new());
        let e820 = [E820Entry::ram(0, 0x0800_0000)];
        let info = load_kernel(&img, "console=ttyS0", &e820, None, None, |gpa, b| {
            writes.borrow_mut().push((gpa, b.to_vec()));
            Ok(())
        })
        .unwrap();

        assert_eq!(info.entry_point, layout::HIGH_RAM_START + 0x200);
        assert_eq!(info.zero_page, layout::ZERO_PAGE_START);

        // Find the zero-page write and verify e820 entry count + cmdline ptr.
        let w = writes.borrow();
        let (_, zp) = w
            .iter()
            .find(|(gpa, _)| *gpa == layout::ZERO_PAGE_START)
            .unwrap();
        assert_eq!(zp[BP_E820_ENTRIES], 1);
        let cmd_ptr = u32::from_le_bytes(zp[OFF_CMD_LINE_PTR..OFF_CMD_LINE_PTR + 4].try_into().unwrap());
        assert_eq!(cmd_ptr, layout::CMDLINE_START as u32);
        assert_eq!(zp[OFF_TYPE_OF_LOADER], 0xff);
    }
}
