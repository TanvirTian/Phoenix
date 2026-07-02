//! The `Vm` struct: owns guest memory, the device Bus, and vCPU thread handles.
//!
//! Threading model (§1.1):
//! * Each vCPU runs its `KVM_RUN` loop on a dedicated `std::thread` (sync).
//! * vCPU threads emit [`VmEvent`]s onto a `std::sync::mpsc::Sender`.
//! * A per-VM bridging tokio task (spawned by the manager) drains that channel
//!   and forwards events into the async control plane. vCPU threads NEVER touch
//!   a tokio channel or an async lock directly.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use vmm_devices::bus::Bus;

use crate::control::protocol::VmEvent;
use crate::vm::state::VmState;

/// Configuration captured at creation time.
#[derive(Debug, Clone)]
pub struct VmConfig {
    pub id: String,
    pub name: String,
    pub memory_mb: u64,
    pub vcpus: u32,
    pub kernel: Option<String>,
    pub cmdline: String,
    pub disk: Option<String>,
    pub initrd: Option<String>,
    /// Framebuffer geometry (width, height) if a display is requested.
    pub framebuffer: Option<(u32, u32)>,
}

/// A running (or created) virtual machine.
///
/// The device `Bus` is shared (`Arc`) with the vCPU threads that dispatch exits
/// against it. `state` is guarded by a `Mutex` because both the control plane
/// (via the manager) and the bridging task may read/update it.
pub struct Vm {
    pub config: VmConfig,
    pub state: Mutex<VmState>,
    pub bus: Arc<Bus>,
    /// vCPU worker thread handles (joined on stop).
    vcpu_threads: Mutex<Vec<JoinHandle<()>>>,
    /// Sync side of the vCPU->control event bridge. Cloned into each vCPU thread.
    event_tx: Sender<VmEvent>,
    /// Held until the bridging task takes it (see `take_event_rx`).
    event_rx: Mutex<Option<Receiver<VmEvent>>>,
    /// Cooperative stop flag polled by vCPU loops.
    stop_flag: Arc<std::sync::atomic::AtomicBool>,
    /// Live KVM run handle once the VM is started (Phase 2+).
    running: Mutex<Option<crate::vm::boot::RunningVm>>,
}

impl Vm {
    pub fn new(config: VmConfig, bus: Bus) -> Self {
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        Self {
            config,
            state: Mutex::new(VmState::Created),
            bus: Arc::new(bus),
            vcpu_threads: Mutex::new(Vec::new()),
            event_tx,
            event_rx: Mutex::new(Some(event_rx)),
            stop_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            running: Mutex::new(None),
        }
    }

    /// Store the live run handle after a successful boot.
    pub fn set_running(&self, handle: crate::vm::boot::RunningVm) {
        *self.running.lock().unwrap() = Some(handle);
    }

    /// Framebuffer info for the GUI: `(raw_fd, width, height, size_bytes)`.
    /// The fd is owned by the running VM; the caller must only `dup`/send it
    /// (e.g. via SCM_RIGHTS), never close it.
    pub fn framebuffer_info(&self) -> Option<(std::os::fd::RawFd, u32, u32, usize)> {
        let guard = self.running.lock().unwrap();
        let rv = guard.as_ref()?;
        let fb = rv.framebuffer.as_ref()?;
        Some((fb.raw_fd(), fb.geometry.width, fb.geometry.height, fb.size))
    }

    /// Feed host serial input (keystrokes) to the guest UART, if running.
    pub fn feed_serial(&self, data: &[u8]) -> bool {
        match &*self.running.lock().unwrap() {
            Some(rv) => {
                rv.feed_serial(data);
                true
            }
            None => false,
        }
    }

    /// Take ownership of the receiving end of the event bridge. The manager
    /// calls this once and moves it into a tokio bridging task.
    pub fn take_event_rx(&self) -> Option<Receiver<VmEvent>> {
        self.event_rx.lock().unwrap().take()
    }

    /// Clone a sender for a vCPU thread (or a device) to push events.
    pub fn event_sender(&self) -> Sender<VmEvent> {
        self.event_tx.clone()
    }

    pub fn stop_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.stop_flag.clone()
    }

    /// Request all vCPU loops to stop at the next iteration.
    pub fn request_stop(&self) {
        self.stop_flag
            .store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(rv) = &*self.running.lock().unwrap() {
            rv.stop();
        }
    }

    /// Register a spawned vCPU thread handle.
    pub fn register_vcpu_thread(&self, handle: JoinHandle<()>) {
        self.vcpu_threads.lock().unwrap().push(handle);
    }

    /// Attempt a state transition, returning the new state on success.
    pub fn transition(&self, to: VmState) -> Result<VmState, crate::vm::state::IllegalTransition> {
        let mut guard = self.state.lock().unwrap();
        if guard.can_transition_to(to) {
            *guard = to;
            // Best-effort notify subscribers of the new state.
            let _ = self.event_tx.send(VmEvent::StateChanged(to.as_str().to_string()));
            Ok(to)
        } else {
            Err(crate::vm::state::IllegalTransition { from: *guard, to })
        }
    }

    pub fn current_state(&self) -> VmState {
        *self.state.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> VmConfig {
        VmConfig {
            id: "vm-1".into(),
            name: "t".into(),
            memory_mb: 128,
            vcpus: 1,
            kernel: None,
            cmdline: "console=ttyS0".into(),
            disk: None,
            initrd: None,
            framebuffer: None,
        }
    }

    #[test]
    fn event_rx_is_takeable_once() {
        let vm = Vm::new(cfg(), Bus::new());
        assert!(vm.take_event_rx().is_some());
        assert!(vm.take_event_rx().is_none());
    }

    #[test]
    fn transition_emits_event() {
        let vm = Vm::new(cfg(), Bus::new());
        let rx = vm.take_event_rx().unwrap();
        vm.transition(VmState::Booting).unwrap();
        let ev = rx.recv().unwrap();
        assert_eq!(ev, VmEvent::StateChanged("Booting".into()));
    }

    #[test]
    fn illegal_transition_errs() {
        let vm = Vm::new(cfg(), Bus::new());
        assert!(vm.transition(VmState::Running).is_err());
    }
}
