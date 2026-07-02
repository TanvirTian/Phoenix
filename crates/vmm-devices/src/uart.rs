//! 16550 UART (serial console), Phase 2 (§4).
//!
//! Enough of the register model to make the Linux `8250`/`serial8250` driver
//! happy: THR/RBR, IER, IIR/FCR, LCR, MCR, LSR, MSR, SCR and the DLAB divisor
//! latch. Bytes the guest writes to THR are forwarded to an output sink
//! (the daemon wires this to a `SerialOutput` VmEvent, §3.3). Bytes injected
//! from the host (keystrokes) are queued and delivered via RBR.
//!
//! Interrupts: the driver switches to interrupt-driven TX once initialized. It
//! writes a byte, enables the "transmit holding register empty" interrupt (IER
//! bit 1), and waits for the UART's IRQ to fire. If we never raise it the guest
//! blocks forever mid-write. So we compute the pending interrupt and pulse an
//! injected [`IrqLine`] (the daemon backs it with KVM `set_irq_line` on IRQ 4).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::device::Device;

// Register offsets (DLAB=0 unless noted).
const REG_DATA: u64 = 0; // RBR (read) / THR (write); DLL when DLAB=1
const REG_IER: u64 = 1; // Interrupt Enable; DLM when DLAB=1
const REG_IIR_FCR: u64 = 2; // IIR (read) / FCR (write)
const REG_LCR: u64 = 3; // Line Control
const REG_MCR: u64 = 4; // Modem Control
const REG_LSR: u64 = 5; // Line Status
const REG_MSR: u64 = 6; // Modem Status
const REG_SCR: u64 = 7; // Scratch

// Line Status Register bits.
const LSR_DATA_READY: u8 = 0x01;
const LSR_THR_EMPTY: u8 = 0x20; // transmit holding register empty
const LSR_TEMT: u8 = 0x40; // transmitter empty
const LCR_DLAB: u8 = 0x80;

// Interrupt Enable Register bits.
const IER_RX_AVAIL: u8 = 0x01; // received data available
const IER_THR_EMPTY: u8 = 0x02; // transmit holding register empty

// Interrupt Identification Register values (bit0=0 means interrupt pending).
const IIR_NO_INT: u8 = 0x01;
const IIR_THR_EMPTY: u8 = 0x02; // TX holding register empty (id=1)
const IIR_RX_AVAIL: u8 = 0x04; // received data available (id=2)
const IIR_FIFO_BITS: u8 = 0xC0; // FIFO enabled bits (16550A)

/// Sink for bytes the guest transmits. Implemented by the daemon to forward to
/// clients. Kept as a trait object so `vmm-devices` has no daemon dependency.
pub trait SerialSink: Send + Sync {
    fn tx(&self, bytes: &[u8]);
}

/// A no-op sink (used in tests / when no console is attached).
pub struct NullSink;
impl SerialSink for NullSink {
    fn tx(&self, _bytes: &[u8]) {}
}

/// Abstraction for asserting/deasserting the UART's guest IRQ line. The daemon
/// backs this with KVM `set_irq_line(COM1_IRQ, active)`; `vmm-devices` stays
/// KVM-agnostic.
pub trait IrqLine: Send + Sync {
    fn set(&self, active: bool);
}

/// A no-op IRQ line (tests / no interrupt controller).
pub struct NullIrq;
impl IrqLine for NullIrq {
    fn set(&self, _active: bool) {}
}

struct Inner {
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    fcr: u8,
    dll: u8,
    dlm: u8,
    /// Whether we are currently asserting the IRQ line (to avoid redundant sets).
    irq_active: bool,
    /// Host->guest input queue (keystrokes).
    rx: VecDeque<u8>,
}

pub struct Uart {
    inner: Mutex<Inner>,
    sink: Arc<dyn SerialSink>,
    irq: Arc<dyn IrqLine>,
}

impl Uart {
    /// Create a UART with a serial sink and no interrupt line.
    pub fn new(sink: Arc<dyn SerialSink>) -> Self {
        Self::with_irq(sink, Arc::new(NullIrq))
    }

    /// Create a UART with a serial sink and an IRQ line.
    pub fn with_irq(sink: Arc<dyn SerialSink>, irq: Arc<dyn IrqLine>) -> Self {
        Self {
            inner: Mutex::new(Inner {
                ier: 0,
                lcr: 0,
                mcr: 0,
                scr: 0,
                fcr: 0,
                dll: 0x0c,
                dlm: 0,
                irq_active: false,
                rx: VecDeque::new(),
            }),
            sink,
            irq,
        }
    }

    /// Queue host input bytes to be read by the guest via RBR, then update the
    /// interrupt state (RX-available may now be pending).
    pub fn enqueue_input(&self, bytes: &[u8]) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.rx.extend(bytes.iter().copied());
        }
        self.update_irq();
    }

    /// Compute whether an interrupt is pending given IER + current state, and
    /// drive the IRQ line accordingly. Called after any state change.
    fn update_irq(&self) {
        let want = {
            let inner = self.inner.lock().unwrap();
            let rx_pending = inner.ier & IER_RX_AVAIL != 0 && !inner.rx.is_empty();
            // TX holding register is always "empty" in our model (we transmit
            // instantly), so a TX interrupt is pending whenever the guest
            // enabled it.
            let tx_pending = inner.ier & IER_THR_EMPTY != 0;
            rx_pending || tx_pending
        };
        let mut inner = self.inner.lock().unwrap();
        if want != inner.irq_active {
            inner.irq_active = want;
            drop(inner);
            self.irq.set(want);
        }
    }

    /// Current IIR value reflecting the highest-priority pending interrupt.
    fn iir(&self, inner: &Inner) -> u8 {
        let rx_pending = inner.ier & IER_RX_AVAIL != 0 && !inner.rx.is_empty();
        let tx_pending = inner.ier & IER_THR_EMPTY != 0;
        let id = if rx_pending {
            IIR_RX_AVAIL
        } else if tx_pending {
            IIR_THR_EMPTY
        } else {
            IIR_NO_INT
        };
        id | IIR_FIFO_BITS
    }
}

impl Device for Uart {
    fn read(&self, offset: u64, data: &mut [u8]) {
        let val: u8 = {
            let mut inner = self.inner.lock().unwrap();
            let dlab = inner.lcr & LCR_DLAB != 0;
            match offset {
                REG_DATA if dlab => inner.dll,
                REG_DATA => inner.rx.pop_front().unwrap_or(0),
                REG_IER if dlab => inner.dlm,
                REG_IER => inner.ier,
                REG_IIR_FCR => self.iir(&inner),
                REG_LCR => inner.lcr,
                REG_MCR => inner.mcr,
                REG_LSR => {
                    let mut lsr = LSR_THR_EMPTY | LSR_TEMT;
                    if !inner.rx.is_empty() {
                        lsr |= LSR_DATA_READY;
                    }
                    lsr
                }
                REG_MSR => 0xB0,
                REG_SCR => inner.scr,
                _ => 0xFF,
            }
        };
        if let Some(first) = data.first_mut() {
            *first = val;
        }
        for b in data.iter_mut().skip(1) {
            *b = 0;
        }
        // Reading RBR or IIR can clear a pending interrupt; re-evaluate.
        self.update_irq();
    }

    fn write(&self, offset: u64, data: &[u8]) {
        let byte = match data.first() {
            Some(b) => *b,
            None => return,
        };
        let mut forward = false;
        {
            let mut inner = self.inner.lock().unwrap();
            let dlab = inner.lcr & LCR_DLAB != 0;
            match offset {
                REG_DATA if dlab => inner.dll = byte,
                REG_DATA => forward = true,
                REG_IER if dlab => inner.dlm = byte,
                REG_IER => inner.ier = byte & 0x0f,
                REG_IIR_FCR => inner.fcr = byte,
                REG_LCR => inner.lcr = byte,
                REG_MCR => inner.mcr = byte,
                REG_SCR => inner.scr = byte,
                _ => {}
            }
        }
        if forward {
            self.sink.tx(&[byte]);
        }
        // Any write may change interrupt state (esp. IER / THR).
        self.update_irq();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CaptureSink(Mutex<Vec<u8>>);
    impl SerialSink for CaptureSink {
        fn tx(&self, bytes: &[u8]) {
            self.0.lock().unwrap().extend_from_slice(bytes);
        }
    }

    struct CountIrq(Mutex<(bool, u32)>); // (current level, rising edges)
    impl IrqLine for CountIrq {
        fn set(&self, active: bool) {
            let mut g = self.0.lock().unwrap();
            if active && !g.0 {
                g.1 += 1;
            }
            g.0 = active;
        }
    }

    #[test]
    fn thr_write_reaches_sink() {
        let sink = Arc::new(CaptureSink(Mutex::new(Vec::new())));
        let uart = Uart::new(sink.clone());
        for &b in b"Hi" {
            uart.write(REG_DATA, &[b]);
        }
        assert_eq!(&*sink.0.lock().unwrap(), b"Hi");
    }

    #[test]
    fn lsr_reports_thr_empty() {
        let uart = Uart::new(Arc::new(NullSink));
        let mut buf = [0u8; 1];
        uart.read(REG_LSR, &mut buf);
        assert_ne!(buf[0] & LSR_THR_EMPTY, 0);
    }

    #[test]
    fn host_input_is_read_back_and_sets_data_ready() {
        let uart = Uart::new(Arc::new(NullSink));
        uart.enqueue_input(b"ab");
        let mut lsr = [0u8; 1];
        uart.read(REG_LSR, &mut lsr);
        assert_ne!(lsr[0] & LSR_DATA_READY, 0);
        let mut d = [0u8; 1];
        uart.read(REG_DATA, &mut d);
        assert_eq!(d[0], b'a');
        uart.read(REG_DATA, &mut d);
        assert_eq!(d[0], b'b');
    }

    #[test]
    fn dlab_switches_data_register_to_divisor_latch() {
        let uart = Uart::new(Arc::new(NullSink));
        uart.write(REG_LCR, &[LCR_DLAB]);
        uart.write(REG_DATA, &[0x01]);
        let mut d = [0u8; 1];
        uart.read(REG_DATA, &mut d);
        assert_eq!(d[0], 0x01);
    }

    #[test]
    fn enabling_thr_interrupt_raises_irq() {
        let irq = Arc::new(CountIrq(Mutex::new((false, 0))));
        let uart = Uart::with_irq(Arc::new(NullSink), irq.clone());
        // Guest enables the TX-empty interrupt.
        uart.write(REG_IER, &[IER_THR_EMPTY]);
        // IRQ should now be asserted (TX is always "empty" in our model).
        assert!(irq.0.lock().unwrap().0, "IRQ line should be active");
        assert!(irq.0.lock().unwrap().1 >= 1, "at least one rising edge");
    }

    #[test]
    fn rx_interrupt_raises_irq_when_enabled() {
        let irq = Arc::new(CountIrq(Mutex::new((false, 0))));
        let uart = Uart::with_irq(Arc::new(NullSink), irq.clone());
        uart.write(REG_IER, &[IER_RX_AVAIL]);
        // No data yet -> no RX interrupt.
        assert!(!irq.0.lock().unwrap().0);
        uart.enqueue_input(b"x");
        assert!(irq.0.lock().unwrap().0, "RX data should assert IRQ");
    }
}
