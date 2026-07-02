//! virtio-mmio transport (VIRTIO 1.x, version 2) for a single virtio-blk device.
//!
//! Implements the MMIO register model the Linux `virtio_mmio` + `virtio_blk`
//! drivers drive during probe and I/O, ties the [`VirtQueue`](super::queue)
//! parser to the [`BlockBackend`](super::block), and raises a guest interrupt
//! (via an injected [`IrqLine`]) when requests complete.
//!
//! Everything here is KVM-agnostic: guest memory is reached through the injected
//! [`GuestAccess`](super::queue::GuestAccess), and interrupts through
//! [`IrqLine`]. The daemon backs both with KVM.

use std::sync::{Arc, Mutex};

use crate::device::Device;
use crate::uart::IrqLine; // reuse the same IRQ abstraction as the UART
use super::block::{
    BlockBackend, BlkReqHeader, SECTOR_SIZE, VIRTIO_BLK_S_IOERR, VIRTIO_BLK_S_OK,
    VIRTIO_BLK_S_UNSUPP, VIRTIO_BLK_T_FLUSH, VIRTIO_BLK_T_IN, VIRTIO_BLK_T_OUT,
};
use super::mmio_regs as reg;
use super::queue::{GuestAccess, QueueConfig, VirtQueue, VIRTQ_DESC_F_WRITE};
use super::{VIRTIO_ID_BLOCK, VIRTIO_MMIO_MAGIC, VIRTIO_MMIO_VERSION};

/// Status register bits (virtio spec).
const STATUS_ACKNOWLEDGE: u32 = 1;
const STATUS_DRIVER: u32 = 2;
const STATUS_DRIVER_OK: u32 = 4;
const STATUS_FEATURES_OK: u32 = 8;

/// Interrupt status bits.
const INT_USED_RING: u32 = 0x1; // used buffer notification

/// VIRTIO_F_VERSION_1 (bit 32) — we advertise a modern device.
const VIRTIO_F_VERSION_1: u64 = 1 << 32;

/// Per-queue driver-programmed state.
#[derive(Default, Clone, Copy)]
struct QueueState {
    num: u16,
    ready: bool,
    desc: u64,
    avail: u64,
    used: u64,
    last_avail_idx: u16,
}

struct Inner {
    status: u32,
    /// Selected queue for the QUEUE_* registers.
    queue_sel: u32,
    /// Feature negotiation: which 32-bit half the driver is reading/writing.
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    interrupt_status: u32,
    queue: QueueState, // virtio-blk uses a single request queue (queue 0)
}

/// virtio-blk over MMIO.
pub struct VirtioBlkMmio {
    inner: Mutex<Inner>,
    backend: Arc<BlockBackend>,
    mem: Arc<dyn GuestAccess>,
    irq: Arc<dyn IrqLine>,
    /// Max queue size we support.
    queue_size_max: u16,
}

impl VirtioBlkMmio {
    pub fn new(
        backend: Arc<BlockBackend>,
        mem: Arc<dyn GuestAccess>,
        irq: Arc<dyn IrqLine>,
    ) -> Self {
        Self {
            inner: Mutex::new(Inner {
                status: 0,
                queue_sel: 0,
                device_features_sel: 0,
                driver_features_sel: 0,
                driver_features: 0,
                interrupt_status: 0,
                queue: QueueState::default(),
            }),
            backend,
            mem,
            irq,
            queue_size_max: 256,
        }
    }

    /// Device feature bits we offer. We only need VERSION_1 for a modern
    /// blk device; block-specific features (SEG_MAX, etc.) are optional.
    fn device_features(&self) -> u64 {
        VIRTIO_F_VERSION_1
    }

    /// The 8-byte virtio-blk config space: `capacity` (u64, in 512-byte sectors).
    fn config_read(&self, offset: u64, data: &mut [u8]) {
        let cap = self.backend.capacity_sectors();
        let bytes = cap.to_le_bytes();
        for (i, b) in data.iter_mut().enumerate() {
            let idx = offset as usize + i;
            *b = *bytes.get(idx).unwrap_or(&0);
        }
    }

    /// Process all available requests on queue 0 (called on QUEUE_NOTIFY).
    fn process_queue(&self) {
        let cfg = {
            let inner = self.inner.lock().unwrap();
            if !inner.queue.ready {
                return;
            }
            QueueConfig {
                size: inner.queue.num,
                desc_table: inner.queue.desc,
                avail_ring: inner.queue.avail,
                used_ring: inner.queue.used,
                ready: true,
            }
        };
        let last_avail = self.inner.lock().unwrap().queue.last_avail_idx;
        let mem = &*self.mem;
        let mut vq = VirtQueue::new(cfg, mem, last_avail);

        let mut any = false;
        while let Some(head) = vq.pop_avail() {
            let written = self.handle_request(&vq, head);
            let _ = vq.add_used(head, written);
            any = true;
        }

        // Persist the consumed avail index.
        self.inner.lock().unwrap().queue.last_avail_idx = vq.last_avail_idx();

        if any {
            let mut inner = self.inner.lock().unwrap();
            inner.interrupt_status |= INT_USED_RING;
            drop(inner);
            self.irq.set(true);
        }
    }

    /// Handle one descriptor chain: [ header(16, RO) | data...(RO/WO) | status(1, WO) ].
    /// Returns the number of bytes written into guest-writable buffers (for the
    /// used ring `len` field).
    fn handle_request<A: GuestAccess + ?Sized>(&self, vq: &VirtQueue<A>, head: u16) -> u32 {
        let chain = vq.chain(head);
        if chain.len() < 2 {
            return 0;
        }

        // Descriptor 0: request header (16 bytes, device-readable).
        let hdr_desc = chain[0];
        let mut hdr_bytes = [0u8; 16];
        if !self.read_guest(hdr_desc.addr, &mut hdr_bytes) {
            return 0;
        }
        let header = match BlkReqHeader::parse(&hdr_bytes) {
            Some(h) => h,
            None => return 0,
        };

        // Last descriptor: status byte (device-writable).
        let status_desc = *chain.last().unwrap();
        // Middle descriptors: data buffers.
        let data_descs = &chain[1..chain.len() - 1];

        let mut status = VIRTIO_BLK_S_OK;
        let mut written: u32 = 0;

        match header.req_type {
            VIRTIO_BLK_T_IN => {
                // Device writes disk data into the (write-only) data buffers.
                let mut sector = header.sector;
                for d in data_descs {
                    if d.flags & VIRTQ_DESC_F_WRITE == 0 {
                        status = VIRTIO_BLK_S_IOERR;
                        break;
                    }
                    let len = d.len as usize;
                    let mut buf = vec![0u8; len];
                    let nsec = (len as u64) / SECTOR_SIZE;
                    if self.backend.read_sectors(sector, &mut buf).is_err() {
                        status = VIRTIO_BLK_S_IOERR;
                        break;
                    }
                    if !self.write_guest(d.addr, &buf) {
                        status = VIRTIO_BLK_S_IOERR;
                        break;
                    }
                    written += len as u32;
                    sector += nsec;
                }
            }
            VIRTIO_BLK_T_OUT => {
                // Device reads guest data and writes it to disk.
                let mut sector = header.sector;
                for d in data_descs {
                    let len = d.len as usize;
                    let mut buf = vec![0u8; len];
                    if !self.read_guest(d.addr, &mut buf) {
                        status = VIRTIO_BLK_S_IOERR;
                        break;
                    }
                    let nsec = (len as u64) / SECTOR_SIZE;
                    if self.backend.write_sectors(sector, &buf).is_err() {
                        status = VIRTIO_BLK_S_IOERR;
                        break;
                    }
                    sector += nsec;
                }
            }
            VIRTIO_BLK_T_FLUSH => {
                if self.backend.flush().is_err() {
                    status = VIRTIO_BLK_S_IOERR;
                }
            }
            _ => status = VIRTIO_BLK_S_UNSUPP,
        }

        // Write the status byte into the last (write-only) descriptor.
        let _ = self.write_guest(status_desc.addr, &[status]);
        // used.len counts device-written bytes: data + 1 status byte.
        written + 1
    }

    // --- guest memory helpers over the byte-oriented GuestAccess ---

    fn read_guest(&self, mut gpa: u64, buf: &mut [u8]) -> bool {
        // GuestAccess is word-oriented; read in u32 chunks with a byte tail.
        let mut i = 0;
        while i + 4 <= buf.len() {
            match self.mem.read_u32(gpa) {
                Some(v) => buf[i..i + 4].copy_from_slice(&v.to_le_bytes()),
                None => return false,
            }
            gpa += 4;
            i += 4;
        }
        while i < buf.len() {
            match self.mem.read_u16(gpa) {
                Some(v) => {
                    buf[i] = (v & 0xff) as u8;
                    if i + 1 < buf.len() {
                        buf[i + 1] = (v >> 8) as u8;
                    }
                }
                None => return false,
            }
            gpa += 2;
            i += 2;
        }
        true
    }

    fn write_guest(&self, mut gpa: u64, buf: &[u8]) -> bool {
        let mut i = 0;
        while i + 4 <= buf.len() {
            let v = u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]);
            if self.mem.write_u32(gpa, v).is_none() {
                return false;
            }
            gpa += 4;
            i += 4;
        }
        while i < buf.len() {
            let lo = buf[i];
            let hi = if i + 1 < buf.len() { buf[i + 1] } else { 0 };
            if self
                .mem
                .write_u16(gpa, (lo as u16) | ((hi as u16) << 8))
                .is_none()
            {
                return false;
            }
            gpa += 2;
            i += 2;
        }
        true
    }
}

impl Device for VirtioBlkMmio {
    fn read(&self, offset: u64, data: &mut [u8]) {
        // Config space is byte-addressable.
        if offset >= reg::CONFIG {
            self.config_read(offset - reg::CONFIG, data);
            return;
        }

        let inner = self.inner.lock().unwrap();
        let val: u32 = match offset {
            reg::MAGIC_VALUE => VIRTIO_MMIO_MAGIC,
            reg::VERSION => VIRTIO_MMIO_VERSION,
            reg::DEVICE_ID => VIRTIO_ID_BLOCK,
            reg::VENDOR_ID => 0x554d_4551, // "QEMU"-ish vendor; any nonzero works
            reg::DEVICE_FEATURES => {
                let f = self.device_features();
                if inner.device_features_sel == 1 {
                    (f >> 32) as u32
                } else {
                    f as u32
                }
            }
            reg::QUEUE_NUM_MAX => self.queue_size_max as u32,
            reg::QUEUE_READY => inner.queue.ready as u32,
            reg::INTERRUPT_STATUS => inner.interrupt_status,
            reg::STATUS => inner.status,
            _ => 0,
        };
        let bytes = val.to_le_bytes();
        for (i, b) in data.iter_mut().enumerate() {
            *b = *bytes.get(i).unwrap_or(&0);
        }
    }

    fn write(&self, offset: u64, data: &[u8]) {
        // Config space writes (rare for blk) are accepted and ignored.
        if offset >= reg::CONFIG {
            return;
        }
        let mut v = [0u8; 4];
        for (i, b) in data.iter().take(4).enumerate() {
            v[i] = *b;
        }
        let val = u32::from_le_bytes(v);

        let notify = {
            let mut inner = self.inner.lock().unwrap();
            match offset {
                reg::DEVICE_FEATURES_SEL => inner.device_features_sel = val,
                reg::DRIVER_FEATURES_SEL => inner.driver_features_sel = val,
                reg::DRIVER_FEATURES => {
                    if inner.driver_features_sel == 1 {
                        inner.driver_features =
                            (inner.driver_features & 0xffff_ffff) | ((val as u64) << 32);
                    } else {
                        inner.driver_features =
                            (inner.driver_features & !0xffff_ffff) | (val as u64);
                    }
                }
                reg::QUEUE_SEL => inner.queue_sel = val,
                reg::QUEUE_NUM => inner.queue.num = val as u16,
                reg::QUEUE_READY => inner.queue.ready = val & 1 != 0,
                reg::QUEUE_DESC_LOW => {
                    inner.queue.desc = (inner.queue.desc & !0xffff_ffff) | val as u64
                }
                reg::QUEUE_DESC_HIGH => {
                    inner.queue.desc = (inner.queue.desc & 0xffff_ffff) | ((val as u64) << 32)
                }
                reg::QUEUE_AVAIL_LOW => {
                    inner.queue.avail = (inner.queue.avail & !0xffff_ffff) | val as u64
                }
                reg::QUEUE_AVAIL_HIGH => {
                    inner.queue.avail = (inner.queue.avail & 0xffff_ffff) | ((val as u64) << 32)
                }
                reg::QUEUE_USED_LOW => {
                    inner.queue.used = (inner.queue.used & !0xffff_ffff) | val as u64
                }
                reg::QUEUE_USED_HIGH => {
                    inner.queue.used = (inner.queue.used & 0xffff_ffff) | ((val as u64) << 32)
                }
                reg::STATUS => {
                    if val == 0 {
                        // Reset.
                        inner.status = 0;
                        inner.queue = QueueState::default();
                        inner.interrupt_status = 0;
                    } else {
                        inner.status = val;
                    }
                }
                reg::INTERRUPT_ACK => {
                    inner.interrupt_status &= !val;
                }
                _ => {}
            }
            // QUEUE_NOTIFY is handled after releasing the lock (process_queue
            // re-locks and does guest memory I/O).
            matches!(offset, reg::QUEUE_NOTIFY)
        };

        // Deassert IRQ once the driver has acked all pending interrupt bits.
        if matches!(offset, reg::INTERRUPT_ACK) {
            let cleared = self.inner.lock().unwrap().interrupt_status == 0;
            if cleared {
                self.irq.set(false);
            }
        }

        if notify {
            self.process_queue();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uart::NullIrq;
    use std::cell::RefCell;
    use std::io::Write as _;

    /// Flat little-endian guest RAM for tests (base 0).
    struct FakeRam(std::sync::Mutex<Vec<u8>>);
    impl GuestAccess for FakeRam {
        fn read_u16(&self, gpa: u64) -> Option<u16> {
            let m = self.0.lock().unwrap();
            let i = gpa as usize;
            Some(u16::from_le_bytes([*m.get(i)?, *m.get(i + 1)?]))
        }
        fn read_u32(&self, gpa: u64) -> Option<u32> {
            let m = self.0.lock().unwrap();
            let i = gpa as usize;
            Some(u32::from_le_bytes(m.get(i..i + 4)?.try_into().unwrap()))
        }
        fn read_u64(&self, gpa: u64) -> Option<u64> {
            let m = self.0.lock().unwrap();
            let i = gpa as usize;
            Some(u64::from_le_bytes(m.get(i..i + 8)?.try_into().unwrap()))
        }
        fn write_u16(&self, gpa: u64, v: u16) -> Option<()> {
            let mut m = self.0.lock().unwrap();
            let i = gpa as usize;
            m.get_mut(i..i + 2)?.copy_from_slice(&v.to_le_bytes());
            Some(())
        }
        fn write_u32(&self, gpa: u64, v: u32) -> Option<()> {
            let mut m = self.0.lock().unwrap();
            let i = gpa as usize;
            m.get_mut(i..i + 4)?.copy_from_slice(&v.to_le_bytes());
            Some(())
        }
    }
    // RefCell isn't Sync; use the Mutex-backed FakeRam above. Silence unused.
    #[allow(dead_code)]
    fn _unused(_: RefCell<u8>) {}

    fn rd32(dev: &VirtioBlkMmio, off: u64) -> u32 {
        let mut b = [0u8; 4];
        dev.read(off, &mut b);
        u32::from_le_bytes(b)
    }
    fn wr32(dev: &VirtioBlkMmio, off: u64, v: u32) {
        dev.write(off, &v.to_le_bytes());
    }

    fn make_disk(sectors: u64) -> (tempfile_path::TempPath, Arc<BlockBackend>) {
        let path = tempfile_path::TempPath::new(format!(
            "vmm-virtio-blk-{}-{}.img",
            std::process::id(),
            fastrand_like()
        ));
        {
            let mut f = std::fs::File::create(path.as_str()).unwrap();
            f.write_all(&vec![0u8; (sectors * SECTOR_SIZE) as usize]).unwrap();
        }
        let be = Arc::new(BlockBackend::open(path.as_str()).unwrap());
        (path, be)
    }

    fn fastrand_like() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos() as u64
    }

    #[test]
    fn magic_version_and_device_id() {
        let (_p, be) = make_disk(8);
        let mem: Arc<dyn GuestAccess> = Arc::new(FakeRam(std::sync::Mutex::new(vec![0u8; 0x1000])));
        let dev = VirtioBlkMmio::new(be, mem, Arc::new(NullIrq));
        assert_eq!(rd32(&dev, reg::MAGIC_VALUE), VIRTIO_MMIO_MAGIC);
        assert_eq!(rd32(&dev, reg::VERSION), VIRTIO_MMIO_VERSION);
        assert_eq!(rd32(&dev, reg::DEVICE_ID), VIRTIO_ID_BLOCK);
    }

    #[test]
    fn capacity_reported_in_config_space() {
        let (_p, be) = make_disk(2048);
        let mem: Arc<dyn GuestAccess> = Arc::new(FakeRam(std::sync::Mutex::new(vec![0u8; 0x1000])));
        let dev = VirtioBlkMmio::new(be, mem, Arc::new(NullIrq));
        let mut b = [0u8; 8];
        dev.read(reg::CONFIG, &mut b);
        assert_eq!(u64::from_le_bytes(b), 2048);
    }

    #[test]
    fn features_advertise_version_1() {
        let (_p, be) = make_disk(8);
        let mem: Arc<dyn GuestAccess> = Arc::new(FakeRam(std::sync::Mutex::new(vec![0u8; 0x1000])));
        let dev = VirtioBlkMmio::new(be, mem, Arc::new(NullIrq));
        // High 32 bits (sel=1) must have bit 0 set (VIRTIO_F_VERSION_1 = bit 32).
        wr32(&dev, reg::DEVICE_FEATURES_SEL, 1);
        assert_eq!(rd32(&dev, reg::DEVICE_FEATURES) & 1, 1);
    }

    #[test]
    fn full_read_request_roundtrip() {
        // Build a disk with a known pattern in sector 0.
        let (_p, be) = make_disk(8);
        be.write_sectors(0, &[0xAB; SECTOR_SIZE as usize]).unwrap();

        // Guest RAM layout: desc@0x100, avail@0x200, used@0x300,
        // header@0x400, data@0x600, status@0x800.
        let ram = Arc::new(FakeRam(std::sync::Mutex::new(vec![0u8; 0x2000])));
        let memdyn: Arc<dyn GuestAccess> = ram.clone();
        let dev = VirtioBlkMmio::new(be, memdyn, Arc::new(NullIrq));

        // Program the queue via MMIO.
        wr32(&dev, reg::QUEUE_SEL, 0);
        wr32(&dev, reg::QUEUE_NUM, 4);
        wr32(&dev, reg::QUEUE_DESC_LOW, 0x100);
        wr32(&dev, reg::QUEUE_AVAIL_LOW, 0x200);
        wr32(&dev, reg::QUEUE_USED_LOW, 0x300);
        wr32(&dev, reg::QUEUE_READY, 1);

        // Request header @0x400: type=IN(0), reserved=0, sector=0.
        ram.write_u32(0x400, VIRTIO_BLK_T_IN).unwrap();
        ram.write_u32(0x404, 0).unwrap();
        ram.write_u32(0x408, 0).unwrap();
        ram.write_u32(0x40c, 0).unwrap();

        // desc[0]: header, 16 bytes, NEXT -> 1
        write_desc(&ram, 0x100, 0, 0x400, 16, super::super::queue::VIRTQ_DESC_F_NEXT, 1);
        // desc[1]: data, 512 bytes, WRITE|NEXT -> 2
        write_desc(
            &ram,
            0x100,
            1,
            0x600,
            SECTOR_SIZE as u32,
            VIRTQ_DESC_F_WRITE | super::super::queue::VIRTQ_DESC_F_NEXT,
            2,
        );
        // desc[2]: status, 1 byte, WRITE
        write_desc(&ram, 0x100, 2, 0x800, 1, VIRTQ_DESC_F_WRITE, 0);

        // avail: idx=1, ring[0]=0
        ram.write_u16(0x202, 1).unwrap();
        ram.write_u16(0x204, 0).unwrap();

        // Notify -> process.
        wr32(&dev, reg::QUEUE_NOTIFY, 0);

        // Status byte should be OK.
        let status = ram.read_u16(0x800).unwrap() & 0xff;
        assert_eq!(status as u8, VIRTIO_BLK_S_OK);
        // Data buffer should now contain the disk pattern.
        assert_eq!(ram.read_u32(0x600).unwrap(), 0xABAB_ABAB);
        // used.idx should have advanced.
        assert_eq!(ram.read_u16(0x302).unwrap(), 1);
        // interrupt status should show the used-ring bit.
        assert_eq!(rd32(&dev, reg::INTERRUPT_STATUS) & INT_USED_RING, INT_USED_RING);
    }

    fn write_desc(
        ram: &FakeRam,
        table: u64,
        idx: u64,
        addr: u64,
        len: u32,
        flags: u16,
        next: u16,
    ) {
        let base = table + idx * 16;
        // addr (u64) via two u32 writes
        ram.write_u32(base, addr as u32).unwrap();
        ram.write_u32(base + 4, (addr >> 32) as u32).unwrap();
        ram.write_u32(base + 8, len).unwrap();
        ram.write_u16(base + 12, flags).unwrap();
        ram.write_u16(base + 14, next).unwrap();
    }
}

/// Tiny temp-path helper (no external deps) for tests.
#[cfg(test)]
mod tempfile_path {
    pub struct TempPath(String);
    impl TempPath {
        pub fn new(name: String) -> Self {
            let mut p = std::env::temp_dir();
            p.push(name);
            TempPath(p.to_string_lossy().into_owned())
        }
        pub fn as_str(&self) -> &str {
            &self.0
        }
    }
    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
}
