//! VirtQueue parsing (Phase 4, §4 — "the hardest part").
//!
//! Split-virtqueue layout per the VIRTIO 1.x spec: a descriptor table, an
//! available ring (driver -> device), and a used ring (device -> driver). This
//! module is a scaffold: the data structures and descriptor-chain walking API
//! are defined; reading/writing actual guest memory is done through an injected
//! accessor so this crate stays KVM-agnostic (no guest-memory type dependency).

/// Descriptor flags.
pub const VIRTQ_DESC_F_NEXT: u16 = 1;
pub const VIRTQ_DESC_F_WRITE: u16 = 2;
pub const VIRTQ_DESC_F_INDIRECT: u16 = 4;

/// One 16-byte split-virtqueue descriptor.
#[derive(Debug, Clone, Copy, Default)]
pub struct Descriptor {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

impl Descriptor {
    pub fn is_write_only(&self) -> bool {
        self.flags & VIRTQ_DESC_F_WRITE != 0
    }
    pub fn has_next(&self) -> bool {
        self.flags & VIRTQ_DESC_F_NEXT != 0
    }
}

/// Guest-physical-memory accessor injected by the daemon so the queue can read
/// descriptors and ring indices without depending on `vm-memory`.
pub trait GuestAccess: Send + Sync {
    fn read_u16(&self, gpa: u64) -> Option<u16>;
    fn read_u32(&self, gpa: u64) -> Option<u32>;
    fn read_u64(&self, gpa: u64) -> Option<u64>;
    fn write_u16(&self, gpa: u64, v: u16) -> Option<()>;
    fn write_u32(&self, gpa: u64, v: u32) -> Option<()>;
}

/// Configuration of a single split virtqueue (addresses set by the driver via
/// the virtio transport registers).
#[derive(Debug, Clone, Copy, Default)]
pub struct QueueConfig {
    pub size: u16,
    pub desc_table: u64,
    pub avail_ring: u64,
    pub used_ring: u64,
    pub ready: bool,
}

/// A live view over a configured virtqueue.
pub struct VirtQueue<'a, A: GuestAccess + ?Sized> {
    cfg: QueueConfig,
    mem: &'a A,
    /// Next available-ring index we have not yet consumed.
    last_avail_idx: u16,
}

impl<'a, A: GuestAccess + ?Sized> VirtQueue<'a, A> {
    pub fn new(cfg: QueueConfig, mem: &'a A, last_avail_idx: u16) -> Self {
        Self {
            cfg,
            mem,
            last_avail_idx,
        }
    }

    pub fn last_avail_idx(&self) -> u16 {
        self.last_avail_idx
    }

    /// Read descriptor `idx` from the descriptor table.
    pub fn descriptor(&self, idx: u16) -> Option<Descriptor> {
        let base = self.cfg.desc_table + (idx as u64) * 16;
        Some(Descriptor {
            addr: self.mem.read_u64(base)?,
            len: self.mem.read_u32(base + 8)?,
            flags: self.mem.read_u16(base + 12)?,
            next: self.mem.read_u16(base + 14)?,
        })
    }

    /// The driver's current available-ring index (avail.idx at offset 2).
    pub fn avail_idx(&self) -> Option<u16> {
        self.mem.read_u16(self.cfg.avail_ring + 2)
    }

    /// Pop the next available descriptor-chain head index, if any.
    pub fn pop_avail(&mut self) -> Option<u16> {
        let avail_idx = self.avail_idx()?;
        if self.last_avail_idx == avail_idx {
            return None; // ring empty
        }
        let ring_slot = self.last_avail_idx % self.cfg.size;
        // avail.ring[] starts at offset 4 (after flags + idx).
        let head = self.mem.read_u16(self.cfg.avail_ring + 4 + (ring_slot as u64) * 2)?;
        self.last_avail_idx = self.last_avail_idx.wrapping_add(1);
        Some(head)
    }

    /// Walk a descriptor chain starting at `head`, collecting descriptors.
    pub fn chain(&self, head: u16) -> Vec<Descriptor> {
        let mut out = Vec::new();
        let mut idx = head;
        // Bound the walk by queue size to avoid loops on a malicious ring.
        for _ in 0..self.cfg.size {
            let Some(d) = self.descriptor(idx) else { break };
            let next = d.next;
            let more = d.has_next();
            out.push(d);
            if !more {
                break;
            }
            idx = next;
        }
        out
    }

    /// Push a completed request into the used ring and bump used.idx.
    pub fn add_used(&mut self, head: u16, len: u32) -> Option<()> {
        let used_idx = self.mem.read_u16(self.cfg.used_ring + 2)?;
        let slot = used_idx % self.cfg.size;
        // used.ring[] elements are 8 bytes (u32 id + u32 len) starting at off 4.
        let elem = self.cfg.used_ring + 4 + (slot as u64) * 8;
        self.mem.write_u32(elem, head as u32)?;
        self.mem.write_u32(elem + 4, len)?;
        self.mem.write_u16(self.cfg.used_ring + 2, used_idx.wrapping_add(1))?;
        Some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A flat little-endian byte array acting as guest memory (base at 0).
    struct FakeMem(Mutex<Vec<u8>>);
    impl FakeMem {
        fn new(size: usize) -> Self {
            Self(Mutex::new(vec![0u8; size]))
        }
    }
    impl GuestAccess for FakeMem {
        fn read_u16(&self, gpa: u64) -> Option<u16> {
            let m = self.0.lock().unwrap();
            let i = gpa as usize;
            Some(u16::from_le_bytes([*m.get(i)?, *m.get(i + 1)?]))
        }
        fn read_u32(&self, gpa: u64) -> Option<u32> {
            let m = self.0.lock().unwrap();
            let i = gpa as usize;
            let s = m.get(i..i + 4)?;
            Some(u32::from_le_bytes(s.try_into().unwrap()))
        }
        fn read_u64(&self, gpa: u64) -> Option<u64> {
            let m = self.0.lock().unwrap();
            let i = gpa as usize;
            let s = m.get(i..i + 8)?;
            Some(u64::from_le_bytes(s.try_into().unwrap()))
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

    #[test]
    fn pops_available_chain_and_records_used() {
        let mem = FakeMem::new(0x1000);
        // Layout: desc@0x100, avail@0x200, used@0x300, size 4.
        let cfg = QueueConfig {
            size: 4,
            desc_table: 0x100,
            avail_ring: 0x200,
            used_ring: 0x300,
            ready: true,
        };
        // desc[0]: addr=0x800, len=16, flags=NEXT, next=1
        mem.write_u32(0x100, 0).unwrap();
        mem.0.lock().unwrap()[0x100..0x108].copy_from_slice(&0x800u64.to_le_bytes());
        mem.write_u32(0x108, 16).unwrap();
        mem.write_u16(0x10c, VIRTQ_DESC_F_NEXT).unwrap();
        mem.write_u16(0x10e, 1).unwrap();
        // desc[1]: addr=0x900, len=8, flags=WRITE, next=0
        mem.0.lock().unwrap()[0x110..0x118].copy_from_slice(&0x900u64.to_le_bytes());
        mem.write_u32(0x118, 8).unwrap();
        mem.write_u16(0x11c, VIRTQ_DESC_F_WRITE).unwrap();
        // avail: idx=1, ring[0]=0
        mem.write_u16(0x202, 1).unwrap();
        mem.write_u16(0x204, 0).unwrap();

        let mut vq = VirtQueue::new(cfg, &mem, 0);
        let head = vq.pop_avail().expect("one available");
        assert_eq!(head, 0);
        let chain = vq.chain(head);
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].addr, 0x800);
        assert!(chain[1].is_write_only());

        vq.add_used(head, 24).unwrap();
        assert_eq!(mem.read_u16(0x302).unwrap(), 1); // used.idx bumped
        assert_eq!(mem.read_u32(0x304).unwrap(), 0); // used.ring[0].id
        assert_eq!(mem.read_u32(0x308).unwrap(), 24); // used.ring[0].len

        assert!(vq.pop_avail().is_none()); // ring drained
    }
}
