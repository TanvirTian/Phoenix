//! Minimal PCI host-bridge *stub* (Phase 2 boot support).
//!
//! We do not implement a PCI bus yet. But Linux probes PCI configuration space
//! very early via the legacy CAM ports 0xCF8 (address) / 0xCFC (data). If those
//! reads return garbage or are unclaimed inconsistently, the scan can wedge.
//!
//! This device claims the CAM ports and always reports "no device present"
//! (all-ones on the data port), so the kernel's enumeration finds nothing and
//! moves on. It is intentionally tiny and KVM-agnostic like every other device.

use std::sync::Mutex;

use crate::device::Device;

/// Legacy PCI config address port.
pub const PCI_CONFIG_ADDRESS: u16 = 0xCF8;
/// Legacy PCI config data port (4 bytes, but accessed at +0..+3).
pub const PCI_CONFIG_DATA: u16 = 0xCFC;

/// Claims ports 0xCF8..=0xCFF (address dword + data dword).
pub struct PciHostBridgeStub {
    /// Last value written to the address port (the selected b/d/f + offset).
    address: Mutex<u32>,
}

impl Default for PciHostBridgeStub {
    fn default() -> Self {
        Self {
            address: Mutex::new(0),
        }
    }
}

impl PciHostBridgeStub {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Device for PciHostBridgeStub {
    fn read(&self, offset: u64, data: &mut [u8]) {
        // offset 0..4 => address port; 4..8 => data port.
        if offset < 4 {
            // Reading back the address register.
            let addr = *self.address.lock().unwrap();
            let bytes = addr.to_le_bytes();
            for (i, b) in data.iter_mut().enumerate() {
                *b = *bytes.get(offset as usize + i).unwrap_or(&0);
            }
        } else {
            // Data port: report "no device" — all ones. A PCI scan reading the
            // vendor ID gets 0xFFFF, which means "no device in this slot".
            data.fill(0xFF);
        }
    }

    fn write(&self, offset: u64, data: &[u8]) {
        if offset < 4 {
            // Update the selected address register (little-endian, partial ok).
            let mut cur = self.address.lock().unwrap();
            let mut bytes = cur.to_le_bytes();
            for (i, b) in data.iter().enumerate() {
                if let Some(slot) = bytes.get_mut(offset as usize + i) {
                    *slot = *b;
                }
            }
            *cur = u32::from_le_bytes(bytes);
        }
        // Writes to the data port are ignored (no devices to configure).
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_port_reports_no_device() {
        let dev = PciHostBridgeStub::new();
        let mut buf = [0u8; 2];
        dev.read(4, &mut buf); // read at data-port offset
        assert_eq!(buf, [0xFF, 0xFF]);
    }

    #[test]
    fn address_register_roundtrips() {
        let dev = PciHostBridgeStub::new();
        dev.write(0, &0x8000_1000u32.to_le_bytes());
        let mut buf = [0u8; 4];
        dev.read(0, &mut buf);
        assert_eq!(u32::from_le_bytes(buf), 0x8000_1000);
    }
}
