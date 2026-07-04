//! VM lifecycle routing: `HashMap<VmId, Arc<Vm>>` plus command handling (§2).
//!
//! The manager is the single owner of all VMs. Client tasks (one per connection)
//! send it commands; it mutates the VM registry and returns responses. Async
//! VM events flow the other way through a broadcast channel that the server
//! fans out to subscribed clients.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{broadcast, Mutex};
use tracing::{info, warn};

use crate::control::protocol::{Command, ResponseBody, VmEvent, VmInfo};
use crate::vm::state::VmState;
use crate::vm::vm::{Vm, VmConfig};
use vmm_devices::bus::Bus;

/// An event tagged with its VM id, broadcast to subscribed clients.
#[derive(Debug, Clone)]
pub struct TaggedEvent {
    pub vm_id: String,
    pub event: VmEvent,
}

pub struct Manager {
    vms: Mutex<HashMap<String, Arc<Vm>>>,
    next_id: Mutex<u64>,
    /// Broadcast bus for async VM events -> subscribed client tasks.
    pub events: broadcast::Sender<TaggedEvent>,
}

#[derive(thiserror::Error, Debug)]
pub enum ManagerError {
    #[error("no such VM: {0}")]
    NotFound(String),
    #[error("illegal transition: {0}")]
    Transition(String),
    #[error("VM operation failed: {0}")]
    Op(String),
}

impl Manager {
    pub fn new() -> Arc<Self> {
        // Large buffer: guest serial output can burst thousands of tiny (often
        // single-byte) SerialOutput events during a boot flood. A small buffer
        // overflows and drops console lines. 64k events comfortably absorbs a
        // full kernel boot before the GUI drains it.
        let (events, _rx) = broadcast::channel(65_536);
        Arc::new(Self {
            vms: Mutex::new(HashMap::new()),
            next_id: Mutex::new(1),
            events,
        })
    }

    async fn alloc_id(&self) -> String {
        let mut n = self.next_id.lock().await;
        let id = format!("vm-{n}");
        *n += 1;
        id
    }

    /// Handle a decoded command, producing a response body.
    pub async fn handle(self: &Arc<Self>, cmd: Command) -> Result<ResponseBody, ManagerError> {
        match cmd {
            Command::CreateVm {
                name,
                memory_mb,
                vcpus,
                kernel,
                cmdline,
                disk,
                initrd,
                framebuffer,
            } => {
                let id = self.alloc_id().await;
                let config = VmConfig {
                    id: id.clone(),
                    name,
                    memory_mb,
                    vcpus,
                    kernel,
                    cmdline: cmdline.unwrap_or_else(|| "console=ttyS0".into()),
                    disk,
                    initrd,
                    framebuffer,
                };
                // Devices are registered here in later phases (UART, virtio...).
                let bus = Bus::new();
                let vm = Arc::new(Vm::new(config, bus));

                // Spawn the sync->async event bridge for this VM.
                self.spawn_event_bridge(&vm);

                self.vms.lock().await.insert(id.clone(), vm);
                info!(vm = %id, "created VM");
                Ok(ResponseBody::Created { id })
            }
            Command::StartVm { id } => {
                let vm = self.get(&id).await?;
                vm.transition(VmState::Booting)
                    .map_err(|e| ManagerError::Transition(e.to_string()))?;
                // Phase 1/2/3: spawn vCPU thread(s) here. In this sandbox
                // (no /dev/kvm) we transition state and emit a synthetic boot
                // event so the control-plane victory condition is observable.
                self.start_vcpus(&vm).await?;
                Ok(ResponseBody::Ok)
            }
            Command::StopVm { id } => {
                let vm = self.get(&id).await?;
                vm.request_stop();
                vm.transition(VmState::Stopped)
                    .map_err(|e| ManagerError::Transition(e.to_string()))?;
                Ok(ResponseBody::Ok)
            }
            Command::PauseVm { id } => {
                let vm = self.get(&id).await?;
                vm.transition(VmState::Paused)
                    .map_err(|e| ManagerError::Transition(e.to_string()))?;
                Ok(ResponseBody::Ok)
            }
            Command::ResumeVm { id } => {
                let vm = self.get(&id).await?;
                vm.transition(VmState::Running)
                    .map_err(|e| ManagerError::Transition(e.to_string()))?;
                Ok(ResponseBody::Ok)
            }
            Command::ListVms => {
                let vms = self.vms.lock().await;
                let list = vms
                    .values()
                    .map(|vm| VmInfo {
                        id: vm.config.id.clone(),
                        name: vm.config.name.clone(),
                        state: vm.current_state().as_str().to_string(),
                        memory_mb: vm.config.memory_mb,
                        vcpus: vm.config.vcpus,
                    })
                    .collect();
                Ok(ResponseBody::VmList { vms: list })
            }
            Command::SendSerialInput { id, data } => {
                let vm = self.get(&id).await?;
                let delivered = vm.feed_serial(&data);
                if !delivered {
                    warn!(vm = %id, "serial input dropped: VM not running");
                }
                Ok(ResponseBody::Ok)
            }
            Command::RequestFramebuffer { id } => {
                // The FD itself is sent out-of-band by the server via SCM_RIGHTS
                // (JSON can't carry an fd). Here we just validate + report the
                // geometry; the server calls `framebuffer_fd` to do the passing.
                let vm = self.get(&id).await?;
                match vm.framebuffer_info() {
                    Some((_, w, h, size)) => Ok(ResponseBody::FramebufferIncoming {
                        width: w,
                        height: h,
                        size: size as u64,
                    }),
                    None => Err(ManagerError::Op("no framebuffer for this VM".into())),
                }
            }
            Command::Subscribe => Ok(ResponseBody::Ok),
        }
    }

    async fn get(&self, id: &str) -> Result<Arc<Vm>, ManagerError> {
        self.vms
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| ManagerError::NotFound(id.to_string()))
    }

    /// The raw framebuffer fd for `id`, for the server to pass via SCM_RIGHTS.
    /// Returns None if the VM has no framebuffer. The fd stays owned by the VM.
    pub async fn framebuffer_fd(&self, id: &str) -> Option<std::os::fd::RawFd> {
        let vm = self.vms.lock().await.get(id).cloned()?;
        vm.framebuffer_info().map(|(fd, _, _, _)| fd)
    }

    /// Drain the VM's sync mpsc event channel into the async broadcast bus.
    fn spawn_event_bridge(self: &Arc<Self>, vm: &Arc<Vm>) {
        let Some(rx) = vm.take_event_rx() else { return };
        let events = self.events.clone();
        let vm_id = vm.config.id.clone();
        // `std::sync::mpsc::Receiver` is blocking; run it on a blocking thread
        // so we never block the async runtime. This is the §3.3 bridge task.
        //
        // Serial output arrives as many tiny (often single-byte) events during a
        // boot flood. We COALESCE consecutive SerialOutput bytes into one event
        // by briefly draining the channel after the first byte, so the async
        // broadcast sees far fewer, larger events (no overflow / dropped lines).
        tokio::task::spawn_blocking(move || {
            use std::sync::mpsc::RecvTimeoutError;
            use std::time::Duration;

            let emit = |ev: VmEvent| {
                let _ = events.send(TaggedEvent {
                    vm_id: vm_id.clone(),
                    event: ev,
                });
            };

            while let Ok(first) = rx.recv() {
                match first {
                    VmEvent::SerialOutput(mut buf) => {
                        // Greedily coalesce further serial bytes that are already
                        // queued or arrive within a tiny window.
                        loop {
                            match rx.recv_timeout(Duration::from_millis(2)) {
                                Ok(VmEvent::SerialOutput(more)) => {
                                    buf.extend_from_slice(&more);
                                    if buf.len() >= 16 * 1024 {
                                        break; // flush large chunks promptly
                                    }
                                }
                                Ok(other) => {
                                    emit(VmEvent::SerialOutput(std::mem::take(&mut buf)));
                                    emit(other);
                                    buf.clear();
                                    break;
                                }
                                Err(RecvTimeoutError::Timeout) => break,
                                Err(RecvTimeoutError::Disconnected) => {
                                    if !buf.is_empty() {
                                        emit(VmEvent::SerialOutput(std::mem::take(&mut buf)));
                                    }
                                    return;
                                }
                            }
                        }
                        if !buf.is_empty() {
                            emit(VmEvent::SerialOutput(buf));
                        }
                    }
                    other => emit(other),
                }
            }
        });
    }

    /// Spawn vCPU worker threads. On a host with /dev/kvm this creates the
    /// hypervisor, memory, vcpus, and runs the KVM_RUN loop through
    /// `handle_exit`. Here it degrades gracefully to a state transition so the
    /// Phase 3 control-plane path is testable without hardware.
    async fn start_vcpus(self: &Arc<Self>, vm: &Arc<Vm>) -> Result<(), ManagerError> {
        // Phase 2: if a kernel is configured, actually boot it on a KVM-backed
        // vCPU thread. The heavy lifting is synchronous (KVM ioctls + thread
        // spawn), so run it on a blocking thread to keep the async runtime free.
        let kernel = vm.config.kernel.clone();
        let cmdline = vm.config.cmdline.clone();
        let memory_mb = vm.config.memory_mb;
        let disk = vm.config.disk.clone();
        let initrd = vm.config.initrd.clone();
        let fb_geom = vm
            .config
            .framebuffer
            .map(|(w, h)| vmm_devices::fb::FbGeometry::new(w, h));
        let event_tx = vm.event_sender();
        let stop = vm.stop_flag();

        match kernel {
            Some(kernel_path) => {
                let vm_id = vm.config.id.clone();
                let result = tokio::task::spawn_blocking(move || {
                    crate::vm::boot::boot_and_run(
                        &kernel_path,
                        &cmdline,
                        memory_mb,
                        initrd.as_deref(),
                        disk.as_deref(),
                        fb_geom,
                        None, // TAP networking wired via the CLI tool for now
                        event_tx,
                        stop,
                    )
                })
                .await
                .map_err(|e| ManagerError::Op(format!("boot task join: {e}")))?;

                match result {
                    Ok(running) => {
                        vm.set_running(running);
                        vm.transition(VmState::Running)
                            .map_err(|e| ManagerError::Transition(e.to_string()))?;
                        info!(vm = %vm_id, "VM booting on KVM vCPU thread");
                        Ok(())
                    }
                    Err(e) => Err(ManagerError::Op(format!("boot failed: {e}"))),
                }
            }
            None => {
                // No kernel: transition to Running and emit an informational
                // serial line so the control-plane path is still observable.
                vm.transition(VmState::Running)
                    .map_err(|e| ManagerError::Transition(e.to_string()))?;
                let tx = vm.event_sender();
                let _ = tx.send(VmEvent::SerialOutput(
                    b"[vmm-daemon] VM started with no kernel configured (idle)\r\n".to_vec(),
                ));
                warn!(vm = %vm.config.id, "no kernel configured; vCPU not spawned");
                Ok(())
            }
        }
    }
}
