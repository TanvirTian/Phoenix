//! virtio-net transport over MMIO (VIRTIO 1.x, version 2) — Phase 8.
//!
//! Mirrors [`VirtioBlkMmio`](super::mmio::VirtioBlkMmio) but presents a network
//! device (device id 1) with **two** virtqueues:
//!
//!   * queue 0 = **receive (RX)** — the driver posts empty, device-writable
//!     buffers; when a frame arrives from the host we fill one and complete it.
//!   * queue 1 = **transmit (TX)** — the driver posts full, device-readable
//!     buffers; on notify we read the frame and hand it to the host backend.
//!
//! Every frame on the wire is prefixed with a 12-byte [`VirtioNetHdr`]. We
//! negotiate no offload features, so the header is all-zero and we simply strip
//! it on TX and prepend a zero one on RX.
//!
//! KVM-agnostic: guest memory via [`GuestAccess`], interrupts via [`IrqLine`],
//! host networking via [`NetBackend`]. The daemon backs all three.

use std::sync::{Arc, Mutex};

use crate::device::Device;
use crate::uart::IrqLine;
use super::mmio_regs as reg;
use super::net::{NetBackend, VirtioNetHdr, VIRTIO_NET_HDR_LEN};
use super::queue::{GuestAccess, QueueConfig, VirtQueue, VIRTQ_DESC_F_WRITE};
use super::{VIRTIO_ID_NET, VIRTIO_MMIO_MAGIC, VIRTIO_MMIO_VERSION};

/// Status register bits (virtio spec).
const STATUS_ACKNOWLEDGE: u32 = 1;
const STATUS_DRIVER: u32 = 2;
const STATUS_DRIVER_OK: u32 = 4;
const STATUS_FEATURES_OK: u32 = 8;

/// Interrupt status bit: used-buffer notification.
const INT_USED_RING: u32 = 0x1;

/// VIRTIO_F_VERSION_1 (bit 32) — advertise a modern device.
const VIRTIO_F_VERSION_1: u64 = 1 << 32;
/// VIRTIO_NET_F_MAC (bit 5) — we provide a fixed MAC in config space.
const VIRTIO_NET_F_MAC: u64 = 1 << 5;
/// VIRTIO_NET_F_STATUS (bit 16) — config space carries a link-status word so we
/// can tell the guest the link is UP (otherwise some drivers/managers treat the
/// interface as having no carrier and tear down its addresses/routes).
const VIRTIO_NET_F_STATUS: u64 = 1 << 16;
/// virtio-net config `status` bit: VIRTIO_NET_S_LINK_UP.
const VIRTIO_NET_S_LINK_UP: u16 = 1;

/// Number of virtqueues (RX=0, TX=1). A control queue would be index 2.
const NUM_QUEUES: usize = 2;
const RX_QUEUE: usize = 0;
const TX_QUEUE: usize = 1;

/// Max Ethernet frame we handle (jumbo-free): 1500 MTU + headers + slack.
const MAX_FRAME: usize = 2048;

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
    queue_sel: u32,
    device_features_sel: u32,
    driver_features_sel: u32,
    driver_features: u64,
    interrupt_status: u32,
    queues: [QueueState; NUM_QUEUES],
}

/// virtio-net over MMIO.
pub struct VirtioNetMmio {
    inner: Mutex<Inner>,
    backend: Arc<dyn NetBackend>,
    mem: Arc<dyn GuestAccess>,
    irq: Arc<dyn IrqLine>,
    mac: [u8; 6],
    queue_size_max: u16,
    /// A single held-back inbound frame we received from the host but could not
    /// yet deliver (no guest RX buffer available). Retried on the next poll.
    pending_rx: Mutex<Option<Vec<u8>>>,
}

impl VirtioNetMmio {
    pub fn new(
        backend: Arc<dyn NetBackend>,
        mem: Arc<dyn GuestAccess>,
        irq: Arc<dyn IrqLine>,
        mac: [u8; 6],
    ) -> Self {
        Self {
            inner: Mutex::new(Inner {
                status: 0,
                queue_sel: 0,
                device_features_sel: 0,
                driver_features_sel: 0,
                driver_features: 0,
                interrupt_status: 0,
                queues: [QueueState::default(); NUM_QUEUES],
            }),
            backend,
            mem,
            irq,
            mac,
            queue_size_max: 256,
            pending_rx: Mutex::new(None),
        }
    }

    /// Device feature bits we offer: modern (VERSION_1) + a fixed MAC.
    fn device_features(&self) -> u64 {
        VIRTIO_F_VERSION_1 | VIRTIO_NET_F_MAC | VIRTIO_NET_F_STATUS
    }

    /// virtio-net config space: 6-byte MAC, then status/queue fields we leave 0.
    fn config_read(&self, offset: u64, data: &mut [u8]) {
        // virtio-net config layout: mac[6] @0..6, then status:u16 @6..8.
        let status = VIRTIO_NET_S_LINK_UP.to_le_bytes();
        for (i, b) in data.iter_mut().enumerate() {
            let idx = offset as usize + i;
            *b = match idx {
                0..=5 => self.mac[idx],
                6 => status[0],
                7 => status[1],
                _ => 0,
            };
        }
    }

    fn queue_cfg(&self, q: usize) -> Option<(QueueConfig, u16)> {
        let inner = self.inner.lock().unwrap();
        let qs = &inner.queues[q];
        if !qs.ready {
            return None;
        }
        Some((
            QueueConfig {
                size: qs.num,
                desc_table: qs.desc,
                avail_ring: qs.avail,
                used_ring: qs.used,
                ready: true,
            },
            qs.last_avail_idx,
        ))
    }

    fn store_last_avail(&self, q: usize, idx: u16) {
        self.inner.lock().unwrap().queues[q].last_avail_idx = idx;
    }

    fn raise_irq(&self) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.interrupt_status |= INT_USED_RING;
        }
        // The virtio IRQ is LEVEL-triggered (see mptable.rs): assert the line
        // and leave it asserted; the guest's INTERRUPT_ACK handler deasserts it
        // once INTERRUPT_STATUS drains to 0. No edge pulse — on a level line a
        // pulse is redundant at best and, combined with the guest's EOI on a
        // separate thread, can drop interrupts via the IOAPIC remote-IRR gate.
        self.irq.set(true);
    }

    /// TX: drain the transmit queue, sending each frame to the host backend.
    /// Called on QUEUE_NOTIFY of the TX queue.
    fn process_tx(&self) {
        let Some((cfg, last_avail)) = self.queue_cfg(TX_QUEUE) else {
            return;
        };
        let mem = &*self.mem;
        let mut vq = VirtQueue::new(cfg, mem, last_avail);

        let mut any = false;
        while let Some(head) = vq.pop_avail() {
            let chain = vq.chain(head);
            // Gather all device-readable bytes across the chain: [hdr | frame].
            let mut buf = Vec::with_capacity(MAX_FRAME);
            for d in &chain {
                // TX descriptors are device-readable (no WRITE flag).
                if d.flags & VIRTQ_DESC_F_WRITE != 0 {
                    continue;
                }
                let mut tmp = vec![0u8; d.len as usize];
                if self.read_guest(d.addr, &mut tmp) {
                    buf.extend_from_slice(&tmp);
                }
                if buf.len() > MAX_FRAME {
                    break;
                }
            }
            // Strip the 12-byte virtio-net header; the rest is the Ethernet frame.
            if buf.len() > VIRTIO_NET_HDR_LEN {
                let frame = &buf[VIRTIO_NET_HDR_LEN..];
                if std::env::var_os("VMM_NET_DEBUG").is_some() {
                    eprintln!("[net] TX frame {} bytes (head={head})", frame.len());
                }
                let _ = self.backend.transmit(frame);
            }
            let _ = vq.add_used(head, 0);
            any = true;
        }

        self.store_last_avail(TX_QUEUE, vq.last_avail_idx());
        if any {
            self.raise_irq();
        }
    }

    /// Poll the host backend once for an inbound frame and, if one is available
    /// (and the guest has posted an RX buffer), deliver it into the RX queue.
    /// Returns `Ok(true)` if a frame was both received and delivered.
    ///
    /// If a frame is read from the host but the guest has no RX buffer posted
    /// yet, we *hold* it in `pending_rx` and retry on the next poll rather than
    /// dropping it — otherwise a burst of replies overruns the guest's posted
    /// buffers and packets are lost (the guest then retransmits forever).
    pub fn poll_rx_once(&self) -> std::io::Result<bool> {
        // First, try to flush any frame we couldn't deliver last time.
        let held = self.pending_rx.lock().unwrap().take();
        if let Some(frame) = held {
            if self.deliver_rx(&frame) {
                return Ok(true);
            }
            // Still no buffer — keep holding it and report "nothing delivered".
            *self.pending_rx.lock().unwrap() = Some(frame);
            return Ok(false);
        }
        // Otherwise pull a fresh frame from the host.
        match self.backend.receive()? {
            Some(frame) => {
                if self.deliver_rx(&frame) {
                    Ok(true)
                } else {
                    // No RX buffer posted yet — hold the frame for next time.
                    *self.pending_rx.lock().unwrap() = Some(frame);
                    Ok(false)
                }
            }
            None => Ok(false),
        }
    }

    /// RX: called by the daemon's poll thread when a frame arrives from the host.
    /// Fills one posted RX descriptor chain with `[virtio-net hdr | frame]` and
    /// completes it. Returns true if a buffer was available and consumed.
    pub fn deliver_rx(&self, frame: &[u8]) -> bool {
        let Some((cfg, last_avail)) = self.queue_cfg(RX_QUEUE) else {
            return false;
        };
        let mem = &*self.mem;
        let mut vq = VirtQueue::new(cfg, mem, last_avail);

        let Some(head) = vq.pop_avail() else {
            // No RX buffer posted yet — caller (poll_rx_once) holds the frame.
            if std::env::var_os("VMM_NET_DEBUG").is_some() {
                eprintln!("[net] RX: no buffer posted (avail_idx={:?})", vq.avail_idx());
            }
            return false;
        };
        let chain = vq.chain(head);

        // Build header + frame and scatter it across the device-writable descs.
        let hdr = VirtioNetHdr::default();
        let mut payload = Vec::with_capacity(VIRTIO_NET_HDR_LEN + frame.len());
        payload.extend_from_slice(&encode_net_hdr(&hdr));
        payload.extend_from_slice(frame);

        let mut written: u32 = 0;
        let mut off = 0usize;
        for d in &chain {
            if d.flags & VIRTQ_DESC_F_WRITE == 0 {
                continue; // RX descriptors must be device-writable
            }
            if off >= payload.len() {
                break;
            }
            let n = std::cmp::min(d.len as usize, payload.len() - off);
            if !self.write_guest(d.addr, &payload[off..off + n]) {
                break;
            }
            off += n;
            written += n as u32;
        }

        let _ = vq.add_used(head, written);
        self.store_last_avail(RX_QUEUE, vq.last_avail_idx());
        if std::env::var_os("VMM_NET_DEBUG").is_some() {
            eprintln!("[net] RX delivered {} bytes (head={head})", frame.len());
        }
        self.raise_irq();
        true
    }

    // --- guest memory helpers over the word-oriented GuestAccess ---

    fn read_guest(&self, mut gpa: u64, buf: &mut [u8]) -> bool {
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

/// Serialize a (zeroed, no-offload) virtio-net header to 12 bytes.
fn encode_net_hdr(h: &VirtioNetHdr) -> [u8; VIRTIO_NET_HDR_LEN] {
    let mut b = [0u8; VIRTIO_NET_HDR_LEN];
    b[0] = h.flags;
    b[1] = h.gso_type;
    b[2..4].copy_from_slice(&h.hdr_len.to_le_bytes());
    b[4..6].copy_from_slice(&h.gso_size.to_le_bytes());
    b[6..8].copy_from_slice(&h.csum_start.to_le_bytes());
    b[8..10].copy_from_slice(&h.csum_offset.to_le_bytes());
    // bytes 10..12 = num_buffers. In VIRTIO 1.0 the 12-byte header is always
    // present, and without VIRTIO_NET_F_MRG_RXBUF each packet occupies exactly
    // ONE descriptor chain — so num_buffers MUST be 1. Leaving it 0 makes the
    // guest treat the packet as malformed and silently drop it.
    b[10..12].copy_from_slice(&1u16.to_le_bytes());
    b
}

impl Device for VirtioNetMmio {
    fn read(&self, offset: u64, data: &mut [u8]) {
        if offset >= reg::CONFIG {
            self.config_read(offset - reg::CONFIG, data);
            return;
        }

        let inner = self.inner.lock().unwrap();
        let val: u32 = match offset {
            reg::MAGIC_VALUE => VIRTIO_MMIO_MAGIC,
            reg::VERSION => VIRTIO_MMIO_VERSION,
            reg::DEVICE_ID => VIRTIO_ID_NET,
            reg::VENDOR_ID => 0x554d_4551, // "QEMU"-ish; any nonzero works
            reg::DEVICE_FEATURES => {
                let f = self.device_features();
                if inner.device_features_sel == 1 {
                    (f >> 32) as u32
                } else {
                    f as u32
                }
            }
            reg::QUEUE_NUM_MAX => self.queue_size_max as u32,
            reg::QUEUE_READY => {
                let q = inner.queue_sel as usize;
                inner.queues.get(q).map(|s| s.ready as u32).unwrap_or(0)
            }
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
        if offset >= reg::CONFIG {
            return; // config space is read-only for us
        }
        let mut v = [0u8; 4];
        for (i, b) in data.iter().take(4).enumerate() {
            v[i] = *b;
        }
        let val = u32::from_le_bytes(v);

        let notify_queue: Option<usize> = {
            let mut inner = self.inner.lock().unwrap();
            let q = inner.queue_sel as usize;
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
                reg::QUEUE_NUM => {
                    if let Some(s) = inner.queues.get_mut(q) {
                        s.num = val as u16;
                    }
                }
                reg::QUEUE_READY => {
                    if let Some(s) = inner.queues.get_mut(q) {
                        s.ready = val & 1 != 0;
                    }
                }
                reg::QUEUE_DESC_LOW => {
                    if let Some(s) = inner.queues.get_mut(q) {
                        s.desc = (s.desc & !0xffff_ffff) | val as u64;
                    }
                }
                reg::QUEUE_DESC_HIGH => {
                    if let Some(s) = inner.queues.get_mut(q) {
                        s.desc = (s.desc & 0xffff_ffff) | ((val as u64) << 32);
                    }
                }
                reg::QUEUE_AVAIL_LOW => {
                    if let Some(s) = inner.queues.get_mut(q) {
                        s.avail = (s.avail & !0xffff_ffff) | val as u64;
                    }
                }
                reg::QUEUE_AVAIL_HIGH => {
                    if let Some(s) = inner.queues.get_mut(q) {
                        s.avail = (s.avail & 0xffff_ffff) | ((val as u64) << 32);
                    }
                }
                reg::QUEUE_USED_LOW => {
                    if let Some(s) = inner.queues.get_mut(q) {
                        s.used = (s.used & !0xffff_ffff) | val as u64;
                    }
                }
                reg::QUEUE_USED_HIGH => {
                    if let Some(s) = inner.queues.get_mut(q) {
                        s.used = (s.used & 0xffff_ffff) | ((val as u64) << 32);
                    }
                }
                reg::STATUS => {
                    if val == 0 {
                        inner.status = 0;
                        inner.queues = [QueueState::default(); NUM_QUEUES];
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
            if matches!(offset, reg::QUEUE_NOTIFY) {
                Some(val as usize)
            } else {
                None
            }
        };

        // Deassert the IRQ once the driver acks all pending interrupt bits.
        if matches!(offset, reg::INTERRUPT_ACK) {
            let cleared = self.inner.lock().unwrap().interrupt_status == 0;
            if cleared {
                self.irq.set(false);
            }
        }

        // A notify on the TX queue drains it. Notifies on RX just mean the guest
        // posted fresh receive buffers; nothing to do synchronously (the daemon
        // poll thread consumes them when host frames arrive).
        if let Some(q) = notify_queue {
            if std::env::var_os("VMM_NET_DEBUG").is_some() {
                eprintln!("[net] QUEUE_NOTIFY q={q}");
            }
            if q == TX_QUEUE {
                self.process_tx();
            }
        }
    }
}

// Keep the status-bit constants referenced (they document the handshake even
// though we don't gate on every one).
#[allow(dead_code)]
const _STATUS_BITS: u32 =
    STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK | STATUS_FEATURES_OK;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::uart::NullIrq;
    use std::sync::Mutex as StdMutex;

    /// Flat little-endian guest RAM for tests (base 0).
    struct FakeRam(StdMutex<Vec<u8>>);
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

    /// A backend that records transmitted frames and can queue RX frames.
    struct CaptureBackend {
        tx: StdMutex<Vec<Vec<u8>>>,
    }
    impl NetBackend for CaptureBackend {
        fn transmit(&self, frame: &[u8]) -> std::io::Result<()> {
            self.tx.lock().unwrap().push(frame.to_vec());
            Ok(())
        }
        fn receive(&self) -> std::io::Result<Option<Vec<u8>>> {
            Ok(None)
        }
    }

    fn rd32(dev: &VirtioNetMmio, off: u64) -> u32 {
        let mut b = [0u8; 4];
        dev.read(off, &mut b);
        u32::from_le_bytes(b)
    }
    fn wr32(dev: &VirtioNetMmio, off: u64, v: u32) {
        dev.write(off, &v.to_le_bytes());
    }
    fn write_desc(ram: &FakeRam, table: u64, idx: u64, addr: u64, len: u32, flags: u16, next: u16) {
        let base = table + idx * 16;
        ram.write_u32(base, addr as u32).unwrap();
        ram.write_u32(base + 4, (addr >> 32) as u32).unwrap();
        ram.write_u32(base + 8, len).unwrap();
        ram.write_u16(base + 12, flags).unwrap();
        ram.write_u16(base + 14, next).unwrap();
    }

    fn make(mem: Arc<dyn GuestAccess>, be: Arc<CaptureBackend>) -> VirtioNetMmio {
        VirtioNetMmio::new(
            be,
            mem,
            Arc::new(NullIrq),
            [0x52, 0x54, 0x00, 0x12, 0x34, 0x56],
        )
    }

    #[test]
    fn magic_version_and_device_id() {
        let mem: Arc<dyn GuestAccess> = Arc::new(FakeRam(StdMutex::new(vec![0u8; 0x1000])));
        let be = Arc::new(CaptureBackend { tx: StdMutex::new(Vec::new()) });
        let dev = make(mem, be);
        assert_eq!(rd32(&dev, reg::MAGIC_VALUE), VIRTIO_MMIO_MAGIC);
        assert_eq!(rd32(&dev, reg::VERSION), VIRTIO_MMIO_VERSION);
        assert_eq!(rd32(&dev, reg::DEVICE_ID), VIRTIO_ID_NET);
    }

    #[test]
    fn mac_in_config_space() {
        let mem: Arc<dyn GuestAccess> = Arc::new(FakeRam(StdMutex::new(vec![0u8; 0x1000])));
        let be = Arc::new(CaptureBackend { tx: StdMutex::new(Vec::new()) });
        let dev = make(mem, be);
        let mut b = [0u8; 6];
        dev.read(reg::CONFIG, &mut b);
        assert_eq!(b, [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
    }

    #[test]
    fn features_advertise_version_1_and_mac() {
        let mem: Arc<dyn GuestAccess> = Arc::new(FakeRam(StdMutex::new(vec![0u8; 0x1000])));
        let be = Arc::new(CaptureBackend { tx: StdMutex::new(Vec::new()) });
        let dev = make(mem, be);
        // Low 32 bits: MAC feature is bit 5, STATUS feature is bit 16.
        let lo = rd32(&dev, reg::DEVICE_FEATURES);
        assert_eq!(lo & (1 << 5), 1 << 5);
        assert_eq!(lo & (1 << 16), 1 << 16);
        // High 32 bits (sel=1): VERSION_1 is bit 32 => bit 0 of the high word.
        wr32(&dev, reg::DEVICE_FEATURES_SEL, 1);
        assert_eq!(rd32(&dev, reg::DEVICE_FEATURES) & 1, 1);
    }

    #[test]
    fn config_reports_link_up() {
        let mem: Arc<dyn GuestAccess> = Arc::new(FakeRam(StdMutex::new(vec![0u8; 0x1000])));
        let be = Arc::new(CaptureBackend { tx: StdMutex::new(Vec::new()) });
        let dev = make(mem, be);
        // status word lives at config offset 6.
        let mut b = [0u8; 2];
        dev.read(reg::CONFIG + 6, &mut b);
        assert_eq!(u16::from_le_bytes(b), VIRTIO_NET_S_LINK_UP);
    }

    #[test]
    fn tx_frame_reaches_backend() {
        // RAM: desc@0x100, avail@0x200, used@0x300, hdr@0x400, frame@0x420.
        let ram = Arc::new(FakeRam(StdMutex::new(vec![0u8; 0x2000])));
        let memdyn: Arc<dyn GuestAccess> = ram.clone();
        let be = Arc::new(CaptureBackend { tx: StdMutex::new(Vec::new()) });
        let dev = make(memdyn, be.clone());

        // Select + program the TX queue (queue index 1).
        wr32(&dev, reg::QUEUE_SEL, TX_QUEUE as u32);
        wr32(&dev, reg::QUEUE_NUM, 4);
        wr32(&dev, reg::QUEUE_DESC_LOW, 0x100);
        wr32(&dev, reg::QUEUE_AVAIL_LOW, 0x200);
        wr32(&dev, reg::QUEUE_USED_LOW, 0x300);
        wr32(&dev, reg::QUEUE_READY, 1);

        // A 12-byte net header (all zero) followed by a 4-byte "frame".
        let frame = [0xDE, 0xAD, 0xBE, 0xEF];
        // desc[0]: header, 12 bytes, NEXT -> 1 (device-readable)
        write_desc(&ram, 0x100, 0, 0x400, 12, super::super::queue::VIRTQ_DESC_F_NEXT, 1);
        // desc[1]: frame, 4 bytes (device-readable, no WRITE)
        write_desc(&ram, 0x100, 1, 0x420, frame.len() as u32, 0, 0);
        for (i, byte) in frame.iter().enumerate() {
            ram.0.lock().unwrap()[0x420 + i] = *byte;
        }
        // avail: idx=1, ring[0]=0
        ram.write_u16(0x202, 1).unwrap();
        ram.write_u16(0x204, 0).unwrap();

        // Notify the TX queue.
        wr32(&dev, reg::QUEUE_NOTIFY, TX_QUEUE as u32);

        let sent = be.tx.lock().unwrap();
        assert_eq!(sent.len(), 1, "one frame should have been transmitted");
        assert_eq!(sent[0], frame, "net header stripped, frame delivered");
        assert_eq!(ram.read_u16(0x302).unwrap(), 1, "used.idx bumped");
    }

    #[test]
    fn rx_frame_delivered_to_guest() {
        // RAM: desc@0x100, avail@0x200, used@0x300, rxbuf@0x400 (2KiB).
        let ram = Arc::new(FakeRam(StdMutex::new(vec![0u8; 0x2000])));
        let memdyn: Arc<dyn GuestAccess> = ram.clone();
        let be = Arc::new(CaptureBackend { tx: StdMutex::new(Vec::new()) });
        let dev = make(memdyn, be);

        // Program the RX queue (index 0) with one big device-writable buffer.
        wr32(&dev, reg::QUEUE_SEL, RX_QUEUE as u32);
        wr32(&dev, reg::QUEUE_NUM, 4);
        wr32(&dev, reg::QUEUE_DESC_LOW, 0x100);
        wr32(&dev, reg::QUEUE_AVAIL_LOW, 0x200);
        wr32(&dev, reg::QUEUE_USED_LOW, 0x300);
        wr32(&dev, reg::QUEUE_READY, 1);

        // desc[0]: 0x400, 1600 bytes, WRITE (device-writable RX buffer)
        write_desc(&ram, 0x100, 0, 0x400, 1600, VIRTQ_DESC_F_WRITE, 0);
        ram.write_u16(0x202, 1).unwrap(); // avail.idx = 1
        ram.write_u16(0x204, 0).unwrap(); // avail.ring[0] = 0

        // Host frame arrives.
        let frame = [0x11, 0x22, 0x33, 0x44, 0x55];
        assert!(dev.deliver_rx(&frame), "RX buffer available");

        // Guest buffer: 12-byte net header (num_buffers=1 at bytes 10..12),
        // then the frame.
        let m = ram.0.lock().unwrap();
        for h in &m[0x400..0x400 + 10] {
            assert_eq!(*h, 0, "net header flags/gso/csum are zeroed");
        }
        assert_eq!(&m[0x40a..0x40c], &1u16.to_le_bytes(), "num_buffers == 1");
        assert_eq!(&m[0x40c..0x40c + 5], &frame, "frame follows the header");
        drop(m);

        assert_eq!(ram.read_u16(0x302).unwrap(), 1, "used.idx bumped");
        // used.len = header (12) + frame (5) = 17
        assert_eq!(ram.read_u32(0x308).unwrap(), (VIRTIO_NET_HDR_LEN + 5) as u32);
    }

    #[test]
    fn rx_with_no_buffer_returns_false() {
        let ram = Arc::new(FakeRam(StdMutex::new(vec![0u8; 0x1000])));
        let memdyn: Arc<dyn GuestAccess> = ram.clone();
        let be = Arc::new(CaptureBackend { tx: StdMutex::new(Vec::new()) });
        let dev = make(memdyn, be);
        // RX queue never made ready => no buffer.
        assert!(!dev.deliver_rx(&[1, 2, 3]));
    }
}
