//! Central VmExit dispatcher (§3.2). Runs *synchronously* on the vCPU thread.
//!
//! This file NEVER touches KVM types. It receives the abstract
//! [`VcpuExit`](vmm_hypervisor::traits::VcpuExit), talks to the device
//! [`Bus`](vmm_devices::bus::Bus), and returns an [`ExitAction`]. The vCPU loop
//! in `vmm-hypervisor::kvm::vcpu_fd` is what actually calls
//! `Vcpu::set_exit_result` to complete a read exit (the §3.2 completion
//! contract). Adding a device never touches this file.

use vmm_devices::bus::Bus;
use vmm_hypervisor::traits::VcpuExit;

/// What the vCPU loop should do after an exit.
#[derive(Debug)]
pub enum ExitAction {
    /// Resume guest execution immediately.
    Continue,
    /// A read completed; the caller must write `data` back into the vCPU via
    /// `Vcpu::set_exit_result` before the next `KVM_RUN`.
    ReadCompleted(Vec<u8>),
    /// The guest requested shutdown; the loop should break.
    Shutdown,
}

#[derive(thiserror::Error, Debug)]
pub enum DispatchError {
    #[error("unhandled vcpu exit: {0}")]
    UnhandledExit(String),
}

/// The one-line dispatch. Adding a device is a Bus registration, not a change
/// here.
pub fn handle_exit(exit: VcpuExit, bus: &Bus) -> Result<ExitAction, DispatchError> {
    match exit {
        VcpuExit::MmioRead { addr, len } => {
            let mut buf = vec![0u8; len];
            bus.read_mmio(addr, &mut buf);
            Ok(ExitAction::ReadCompleted(buf))
        }
        VcpuExit::MmioWrite { addr, data } => {
            bus.write_mmio(addr, &data);
            Ok(ExitAction::Continue)
        }
        VcpuExit::IoIn { port, len } => {
            let mut buf = vec![0u8; len];
            bus.read_pio(port as u64, &mut buf);
            Ok(ExitAction::ReadCompleted(buf))
        }
        VcpuExit::IoOut { port, data } => {
            bus.write_pio(port as u64, &data);
            Ok(ExitAction::Continue)
        }
        VcpuExit::Hlt => {
            // With an in-kernel irqchip + LAPIC, re-enter KVM_RUN promptly:
            // KVM itself handles the halt and wakes the vCPU on the next timer
            // or device interrupt. Yielding (not sleeping) keeps the guest's
            // local-APIC timer advancing, which drives the scheduler tick and
            // process wakeups (e.g. ping's per-packet interval).
            std::thread::yield_now();
            Ok(ExitAction::Continue)
        }
        VcpuExit::Shutdown => Ok(ExitAction::Shutdown),
        other => Err(DispatchError::UnhandledExit(format!("{other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    //! Because `handle_exit` depends only on the KVM-agnostic Bus and VcpuExit,
    //! we can exercise the full dispatch path with zero KVM access — satisfying
    //! the "compile-only + unit tests" verification strategy for this sandbox.
    use super::*;
    use std::sync::{Arc, Mutex};
    use vmm_devices::bus::BusRange;
    use vmm_devices::device::Device;

    struct Sink(Mutex<Vec<u8>>);
    impl Device for Sink {
        fn read(&self, _o: u64, data: &mut [u8]) {
            data.fill(0x42);
        }
        fn write(&self, _o: u64, data: &[u8]) {
            self.0.lock().unwrap().extend_from_slice(data);
        }
    }

    fn bus_with_pio() -> (Bus, Arc<Sink>) {
        let mut bus = Bus::new();
        let dev = Arc::new(Sink(Mutex::new(Vec::new())));
        bus.register_pio(BusRange::new(0x3F8, 8), dev.clone()).unwrap();
        (bus, dev)
    }

    #[test]
    fn io_out_reaches_device() {
        let (bus, dev) = bus_with_pio();
        let act = handle_exit(
            VcpuExit::IoOut {
                port: 0x3F8,
                data: b"X".to_vec(),
            },
            &bus,
        )
        .unwrap();
        assert!(matches!(act, ExitAction::Continue));
        assert_eq!(&*dev.0.lock().unwrap(), b"X");
    }

    #[test]
    fn io_in_returns_completed_read() {
        let (bus, _dev) = bus_with_pio();
        let act = handle_exit(VcpuExit::IoIn { port: 0x3F8, len: 1 }, &bus).unwrap();
        match act {
            ExitAction::ReadCompleted(d) => assert_eq!(d, vec![0x42]),
            _ => panic!("expected ReadCompleted"),
        }
    }

    #[test]
    fn shutdown_maps_to_shutdown_action() {
        let bus = Bus::new();
        assert!(matches!(
            handle_exit(VcpuExit::Shutdown, &bus).unwrap(),
            ExitAction::Shutdown
        ));
    }

    #[test]
    fn unknown_exit_is_error() {
        let bus = Bus::new();
        let err = handle_exit(VcpuExit::Unhandled("weird".into()), &bus).unwrap_err();
        assert!(matches!(err, DispatchError::UnhandledExit(_)));
    }
}
