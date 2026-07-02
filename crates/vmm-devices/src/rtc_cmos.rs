//! Minimal MC146818 RTC / CMOS stub (ports 0x70 index, 0x71 data).
//!
//! Linux polls RTC status register 0x0A very early: it waits for the
//! "update-in-progress" (UIP, bit 7) to be CLEAR before reading time. If reads
//! return 0xFF (our default for unclaimed ports), UIP is always set and the
//! kernel spins forever. This stub returns UIP=0 and plausible BCD time/date
//! values so RTC reads complete and boot proceeds.
//!
//! It is intentionally minimal and KVM-agnostic like every other device.

use std::sync::Mutex;

use crate::device::Device;

pub const RTC_INDEX_PORT: u16 = 0x70;
pub const RTC_DATA_PORT: u16 = 0x71;

// CMOS register indices we answer meaningfully.
const REG_SECONDS: u8 = 0x00;
const REG_MINUTES: u8 = 0x02;
const REG_HOURS: u8 = 0x04;
const REG_DAY_OF_MONTH: u8 = 0x07;
const REG_MONTH: u8 = 0x08;
const REG_YEAR: u8 = 0x09;
const REG_STATUS_A: u8 = 0x0A; // bit7 = UIP (must read 0 to proceed)
const REG_STATUS_B: u8 = 0x0B; // format flags (DM/24h)
const REG_STATUS_C: u8 = 0x0C;
const REG_STATUS_D: u8 = 0x0D; // bit7 = valid RAM/time

pub struct RtcCmos {
    /// Last value written to the index port (selected register), low 7 bits.
    index: Mutex<u8>,
    /// 128 bytes of CMOS RAM.
    ram: Mutex<[u8; 128]>,
}

impl Default for RtcCmos {
    fn default() -> Self {
        let mut ram = [0u8; 128];
        // A fixed, plausible time: 2024-01-01 00:00:00, binary + 24h format.
        ram[REG_SECONDS as usize] = 0;
        ram[REG_MINUTES as usize] = 0;
        ram[REG_HOURS as usize] = 0;
        ram[REG_DAY_OF_MONTH as usize] = 1;
        ram[REG_MONTH as usize] = 1;
        ram[REG_YEAR as usize] = 24;
        ram[REG_STATUS_A as usize] = 0x26; // UIP=0, sane divider/rate bits
        ram[REG_STATUS_B as usize] = 0x02 | 0x04; // 24h mode + binary (DM) format
        ram[REG_STATUS_C as usize] = 0x00;
        ram[REG_STATUS_D as usize] = 0x80; // valid RAM and time
        Self {
            index: Mutex::new(0),
            ram: Mutex::new(ram),
        }
    }
}

impl RtcCmos {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Device for RtcCmos {
    fn read(&self, offset: u64, data: &mut [u8]) {
        // offset 0 => index port (rarely read); offset 1 => data port.
        let val = if offset == 0 {
            *self.index.lock().unwrap()
        } else {
            let idx = (*self.index.lock().unwrap() & 0x7f) as usize;
            let ram = self.ram.lock().unwrap();
            // Status A: always report UIP=0 so the kernel never waits.
            if idx == REG_STATUS_A as usize {
                ram[idx] & 0x7f
            } else {
                ram[idx]
            }
        };
        if let Some(b) = data.first_mut() {
            *b = val;
        }
        for b in data.iter_mut().skip(1) {
            *b = 0;
        }
    }

    fn write(&self, offset: u64, data: &[u8]) {
        let byte = match data.first() {
            Some(b) => *b,
            None => return,
        };
        if offset == 0 {
            // Index port: low 7 bits select register; bit7 is the NMI-disable
            // flag which we ignore.
            *self.index.lock().unwrap() = byte & 0x7f;
        } else {
            let idx = (*self.index.lock().unwrap() & 0x7f) as usize;
            self.ram.lock().unwrap()[idx] = byte;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_a_reports_uip_clear() {
        let rtc = RtcCmos::new();
        // select register 0x0A
        rtc.write(0, &[REG_STATUS_A]);
        let mut buf = [0u8; 1];
        rtc.read(1, &mut buf);
        assert_eq!(buf[0] & 0x80, 0, "UIP must be clear so the kernel proceeds");
    }

    #[test]
    fn time_registers_readable() {
        let rtc = RtcCmos::new();
        rtc.write(0, &[REG_DAY_OF_MONTH]);
        let mut buf = [0u8; 1];
        rtc.read(1, &mut buf);
        assert_eq!(buf[0], 1);
    }

    #[test]
    fn ram_roundtrips() {
        let rtc = RtcCmos::new();
        rtc.write(0, &[0x50]); // scratch CMOS byte
        rtc.write(1, &[0xAB]);
        rtc.write(0, &[0x50]);
        let mut buf = [0u8; 1];
        rtc.read(1, &mut buf);
        assert_eq!(buf[0], 0xAB);
    }
}
