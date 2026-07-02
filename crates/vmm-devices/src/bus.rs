//! The device **Bus** (§3.1): address-range dispatch for MMIO and PIO.
//!
//! The exit dispatcher (`vmm-daemon::vm::exit`) calls the four
//! `read_*`/`write_*` methods and never inspects individual devices. Adding a
//! device is a `register_*` call — it never touches `exit.rs`.

use std::sync::Arc;

use crate::device::Device;

/// Half-open guest address range `[start, end)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusRange {
    pub start: u64,
    pub end: u64,
}

impl BusRange {
    pub fn new(start: u64, len: u64) -> Self {
        Self {
            start,
            end: start + len,
        }
    }

    #[inline]
    fn contains(&self, addr: u64) -> bool {
        addr >= self.start && addr < self.end
    }
}

#[derive(thiserror::Error, Debug)]
pub enum BusError {
    #[error("device range [{new_start:#x},{new_end:#x}) overlaps existing [{ex_start:#x},{ex_end:#x})")]
    Overlap {
        new_start: u64,
        new_end: u64,
        ex_start: u64,
        ex_end: u64,
    },
}

#[derive(Default)]
pub struct Bus {
    mmio_devices: Vec<(BusRange, Arc<dyn Device>)>,
    pio_devices: Vec<(BusRange, Arc<dyn Device>)>,
}

impl Bus {
    pub fn new() -> Self {
        Self::default()
    }

    fn insert(
        list: &mut Vec<(BusRange, Arc<dyn Device>)>,
        range: BusRange,
        dev: Arc<dyn Device>,
    ) -> Result<(), BusError> {
        for (ex, _) in list.iter() {
            let overlaps = range.start < ex.end && ex.start < range.end;
            if overlaps {
                return Err(BusError::Overlap {
                    new_start: range.start,
                    new_end: range.end,
                    ex_start: ex.start,
                    ex_end: ex.end,
                });
            }
        }
        list.push((range, dev));
        Ok(())
    }

    /// Register an MMIO device occupying `range`.
    pub fn register_mmio(&mut self, range: BusRange, dev: Arc<dyn Device>) -> Result<(), BusError> {
        Self::insert(&mut self.mmio_devices, range, dev)
    }

    /// Register a port-I/O device occupying `range` (ports as u64 addresses).
    pub fn register_pio(&mut self, range: BusRange, dev: Arc<dyn Device>) -> Result<(), BusError> {
        Self::insert(&mut self.pio_devices, range, dev)
    }

    fn find<'a>(
        list: &'a [(BusRange, Arc<dyn Device>)],
        addr: u64,
    ) -> Option<(u64, &'a Arc<dyn Device>)> {
        list.iter()
            .find(|(r, _)| r.contains(addr))
            .map(|(r, d)| (addr - r.start, d))
    }

    pub fn read_mmio(&self, addr: u64, data: &mut [u8]) {
        match Self::find(&self.mmio_devices, addr) {
            Some((offset, dev)) => dev.read(offset, data),
            None => data.fill(0xFF), // unclaimed reads return 0xFF (§3.1)
        }
    }

    pub fn write_mmio(&self, addr: u64, data: &[u8]) {
        if let Some((offset, dev)) = Self::find(&self.mmio_devices, addr) {
            dev.write(offset, data);
        }
        // Unclaimed writes are silently dropped, matching real hardware behavior
        // for an unbacked MMIO hole.
    }

    pub fn read_pio(&self, port: u64, data: &mut [u8]) {
        match Self::find(&self.pio_devices, port) {
            Some((offset, dev)) => dev.read(offset, data),
            None => data.fill(0xFF),
        }
    }

    pub fn write_pio(&self, port: u64, data: &[u8]) {
        if let Some((offset, dev)) = Self::find(&self.pio_devices, port) {
            dev.write(offset, data);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// A device that records writes and returns a fixed byte on reads.
    struct Recorder {
        marker: u8,
        writes: Mutex<Vec<(u64, Vec<u8>)>>,
    }
    impl Device for Recorder {
        fn read(&self, _offset: u64, data: &mut [u8]) {
            data.fill(self.marker);
        }
        fn write(&self, offset: u64, data: &[u8]) {
            self.writes.lock().unwrap().push((offset, data.to_vec()));
        }
    }

    #[test]
    fn unclaimed_mmio_read_returns_0xff() {
        let bus = Bus::new();
        let mut buf = [0u8; 4];
        bus.read_mmio(0xdead_beef, &mut buf);
        assert_eq!(buf, [0xFF; 4]);
    }

    #[test]
    fn dispatch_translates_offset() {
        let mut bus = Bus::new();
        let rec = Arc::new(Recorder {
            marker: 0xAB,
            writes: Mutex::new(Vec::new()),
        });
        bus.register_mmio(BusRange::new(0x1000, 0x100), rec.clone())
            .unwrap();

        let mut buf = [0u8; 2];
        bus.read_mmio(0x1004, &mut buf);
        assert_eq!(buf, [0xAB, 0xAB]);

        bus.write_mmio(0x1008, &[1, 2, 3]);
        let w = rec.writes.lock().unwrap();
        assert_eq!(w[0], (0x8, vec![1, 2, 3])); // offset is device-relative
    }

    #[test]
    fn overlap_is_rejected() {
        let mut bus = Bus::new();
        let d = Arc::new(Recorder {
            marker: 0,
            writes: Mutex::new(Vec::new()),
        });
        bus.register_mmio(BusRange::new(0x1000, 0x100), d.clone())
            .unwrap();
        let err = bus
            .register_mmio(BusRange::new(0x1080, 0x100), d.clone())
            .unwrap_err();
        assert!(matches!(err, BusError::Overlap { .. }));
    }

    #[test]
    fn pio_and_mmio_are_separate_spaces() {
        let mut bus = Bus::new();
        let d = Arc::new(Recorder {
            marker: 0x11,
            writes: Mutex::new(Vec::new()),
        });
        // Same numeric address in PIO and MMIO must not collide.
        bus.register_mmio(BusRange::new(0x3F8, 8), d.clone()).unwrap();
        bus.register_pio(BusRange::new(0x3F8, 8), d.clone()).unwrap();
        let mut buf = [0u8; 1];
        bus.read_pio(0x3F8, &mut buf);
        assert_eq!(buf, [0x11]);
    }
}
