//! Phase 2 boot path: create a KVM-backed VM, load a bzImage, wire the 16550
//! UART onto the device Bus, and run the vCPU `KVM_RUN` loop on a dedicated
//! `std::thread` that dispatches through `handle_exit` / `set_exit_result`.
//!
//! This module is the *only* place that ties `vmm-hypervisor` (KVM) to the
//! synchronous exit dispatcher and the device Bus. It never `.await`s.

use std::sync::mpsc::Sender;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tracing::info;

use vmm_boot::layout;
use vmm_boot::linux::{self, E820Entry};
use vmm_devices::bus::{Bus, BusRange};
use vmm_devices::uart::{SerialSink, Uart};
use vmm_hypervisor::kvm::memory::GuestRam;
use vmm_hypervisor::traits::{Hypervisor, LongModeEntry, Vcpu, Vm as _};
use vmm_hypervisor::KvmHypervisor;

use crate::control::protocol::VmEvent;
use crate::vm::exit::{handle_exit, ExitAction};

#[derive(thiserror::Error, Debug)]
pub enum BootRunError {
    #[error("hypervisor: {0}")]
    Hypervisor(#[from] vmm_hypervisor::traits::HypervisorError),
    #[error("boot loader: {0}")]
    Boot(#[from] vmm_boot::linux::BootError),
    #[error("bus: {0}")]
    Bus(#[from] vmm_devices::bus::BusError),
    #[error("block device: {0}")]
    Block(#[from] vmm_devices::virtio::block::BlockError),
    #[error("framebuffer: {0}")]
    Framebuffer(#[from] crate::vm::framebuffer::FbError),
    #[error("failed to read kernel image {path}: {source}")]
    KernelRead {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("no kernel configured for this VM")]
    NoKernel,
}

/// Forwards guest serial output into the VM's event channel (§3.3 bridge).
struct EventSink {
    tx: Sender<VmEvent>,
}
impl SerialSink for EventSink {
    fn tx(&self, bytes: &[u8]) {
        // Best-effort: if the bridge is gone the VM is tearing down.
        let _ = self.tx.send(VmEvent::SerialOutput(bytes.to_vec()));
    }
}

/// Backs a device IRQ line with KVM's in-kernel IRQ chip.
struct KvmIrqLine {
    vm: Arc<vmm_hypervisor::kvm::vm_fd::KvmVm>,
    irq: u32,
}
impl vmm_devices::uart::IrqLine for KvmIrqLine {
    fn set(&self, active: bool) {
        // Best-effort: ignore errors (e.g. during teardown).
        let _ = self.vm.set_irq_line(self.irq, active);
    }
}

/// Adapts guest RAM to the virtqueue's `GuestAccess` (little-endian word I/O).
struct RamAccess {
    ram: Arc<GuestRam>,
}
impl vmm_devices::virtio::queue::GuestAccess for RamAccess {
    fn read_u16(&self, gpa: u64) -> Option<u16> {
        let b = self.ram.read_vec(gpa, 2).ok()?;
        Some(u16::from_le_bytes([b[0], b[1]]))
    }
    fn read_u32(&self, gpa: u64) -> Option<u32> {
        let b = self.ram.read_vec(gpa, 4).ok()?;
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn read_u64(&self, gpa: u64) -> Option<u64> {
        let b = self.ram.read_vec(gpa, 8).ok()?;
        Some(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    fn write_u16(&self, gpa: u64, v: u16) -> Option<()> {
        self.ram.write_slice(gpa, &v.to_le_bytes()).ok()
    }
    fn write_u32(&self, gpa: u64, v: u32) -> Option<()> {
        self.ram.write_slice(gpa, &v.to_le_bytes()).ok()
    }
}

/// Handle returned to the caller so it can feed serial input and stop the VM.
#[derive(Clone)]
pub struct RunningVm {
    /// The UART, so `SendSerialInput` can enqueue host keystrokes.
    pub uart: Arc<Uart>,
    /// The shared framebuffer (if enabled), so the control plane can hand its
    /// FD to the GUI over SCM_RIGHTS.
    pub framebuffer: Option<Arc<crate::vm::framebuffer::SharedFramebuffer>>,
    stop: Arc<AtomicBool>,
}

impl RunningVm {
    pub fn feed_serial(&self, data: &[u8]) {
        self.uart.enqueue_input(data);
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Build and launch a KVM VM booting `kernel_path`. Spawns the vCPU thread and
/// returns a handle immediately. Guest serial output flows to `event_tx`.
#[allow(clippy::too_many_arguments)]
pub fn boot_and_run(
    kernel_path: &str,
    cmdline: &str,
    memory_mb: u64,
    initrd_path: Option<&str>,
    disk_path: Option<&str>,
    fb_geometry: Option<vmm_devices::fb::FbGeometry>,
    event_tx: Sender<VmEvent>,
    stop: Arc<AtomicBool>,
) -> Result<RunningVm, BootRunError> {
    // --- 1. Hypervisor + VM + arch setup ---
    let hv = KvmHypervisor::new()?;
    let vm = Arc::new(hv.create_vm()?);
    vm.arch_setup()?;

    // --- 2. Guest RAM ---
    let mem_size = (memory_mb as usize) * 1024 * 1024;
    let ram = Arc::new(GuestRam::new_single(layout::RAM_START, mem_size, 0)?);
    for r in ram.regions() {
        // SAFETY: `ram` is kept alive for the VM's lifetime (moved into the
        // vCPU thread closure below).
        unsafe {
            vm.set_user_memory_region(r.slot, r.guest_phys_addr, r.size, r.host_addr)?;
        }
    }

    // --- 2b. Shared framebuffer (Phase 6) ---
    // Create a memfd, map it in the daemon, and register that same mapping as a
    // guest memslot at the FB aperture. Guest pixel writes land directly in the
    // memfd (zero copy); the FD is later passed to the GUI over SCM_RIGHTS.
    let framebuffer = match fb_geometry {
        Some(geom) => {
            let fb = Arc::new(crate::vm::framebuffer::SharedFramebuffer::new(geom)?);
            // SAFETY: `fb` owns the memfd + mapping for the VM's lifetime (it is
            // moved into the returned RunningVm / kept alive by the daemon).
            unsafe {
                vm.set_user_memory_region(
                    1, // memslot 1 (slot 0 is main RAM)
                    layout::FRAMEBUFFER_BASE,
                    fb.size as u64,
                    fb.host_addr,
                )?;
            }
            info!(
                base = format_args!("{:#x}", layout::FRAMEBUFFER_BASE),
                w = geom.width,
                h = geom.height,
                "framebuffer registered"
            );
            Some(fb)
        }
        None => None,
    };

    // --- 3. Boot GDT + identity page tables ---
    ram.write_boot_gdt(layout::BOOT_GDT_START)?;
    ram.write_identity_page_tables(
        layout::PML4_START,
        layout::PDPTE_START,
        layout::PDE_START,
    )?;

    // --- 4. Load the kernel ---
    let image = std::fs::read(kernel_path).map_err(|e| BootRunError::KernelRead {
        path: kernel_path.to_string(),
        source: e,
    })?;
    // Optional initrd (initramfs) image.
    let initrd_bytes = match initrd_path {
        Some(p) => Some(std::fs::read(p).map_err(|e| BootRunError::KernelRead {
            path: p.to_string(),
            source: e,
        })?),
        None => None,
    };
    // Advertise RAM below the MMIO hole as one e820 region.
    let usable = (mem_size as u64).min(layout::VIRTIO_MMIO_BASE);
    let e820 = [E820Entry::ram(layout::RAM_START, usable)];
    // If a framebuffer exists, describe it in screen_info so the guest creates
    // /dev/fb0 (the x86 "VESA VLFB" firmware-handoff path; no device tree).
    let fb_info = framebuffer.as_ref().map(|fb| vmm_boot::linux::FbInfo {
        base: layout::FRAMEBUFFER_BASE,
        width: fb.geometry.width,
        height: fb.geometry.height,
        bpp: (vmm_devices::fb::BYTES_PER_PIXEL * 8) as u32,
    });
    let boot_info = {
        let ram2 = ram.clone();
        linux::load_kernel(
            &image,
            cmdline,
            &e820,
            initrd_bytes.as_deref(),
            fb_info,
            move |gpa, bytes| ram2.write_slice(gpa, bytes).map_err(|e| e.to_string()),
        )?
    };
    info!(entry = format_args!("{:#x}", boot_info.entry_point), "kernel loaded");

    // --- 5. Devices: 16550 UART on COM1 (PIO 0x3F8) ---
    // The UART can raise IRQ 4 for interrupt-driven console I/O. This is needed
    // for a fully interactive shell, but if it ever misbehaves you can fall back
    // to polled mode with VMM_NO_UART_IRQ=1 to isolate problems.
    let uart = if std::env::var_os("VMM_NO_UART_IRQ").is_some() {
        Arc::new(Uart::new(Arc::new(EventSink {
            tx: event_tx.clone(),
        })))
    } else {
        let uart_irq = Arc::new(KvmIrqLine {
            vm: vm.clone(),
            irq: layout::COM1_IRQ,
        });
        Arc::new(Uart::with_irq(
            Arc::new(EventSink {
                tx: event_tx.clone(),
            }),
            uart_irq,
        ))
    };
    let mut bus = Bus::new();
    bus.register_pio(
        BusRange::new(layout::COM1_PORT_BASE as u64, layout::COM1_PORT_SIZE as u64),
        uart.clone() as Arc<dyn vmm_devices::device::Device>,
    )?;
    // PCI host-bridge stub on the legacy CAM ports 0xCF8..0xD00 so the kernel's
    // early PCI enumeration finds "no devices" and moves on instead of wedging.
    bus.register_pio(
        BusRange::new(vmm_devices::pci_stub::PCI_CONFIG_ADDRESS as u64, 8),
        Arc::new(vmm_devices::pci_stub::PciHostBridgeStub::new())
            as Arc<dyn vmm_devices::device::Device>,
    )?;
    // RTC/CMOS stub on ports 0x70/0x71. Without it the kernel spins forever
    // polling RTC status register 0x0A waiting for the update-in-progress bit
    // (which reads 0xFF => always "updating") to clear.
    bus.register_pio(
        BusRange::new(vmm_devices::rtc_cmos::RTC_INDEX_PORT as u64, 2),
        Arc::new(vmm_devices::rtc_cmos::RtcCmos::new())
            as Arc<dyn vmm_devices::device::Device>,
    )?;

    // virtio-blk over MMIO (Phase 4). Only present when a disk image is given.
    // Registered at VIRTIO_MMIO_BASE; the guest is told about it via the kernel
    // cmdline `virtio_mmio.device=` parameter (added in the manager/CLI).
    if let Some(path) = disk_path {
        let backend = Arc::new(vmm_devices::virtio::block::BlockBackend::open(path)?);
        let mem_access: Arc<dyn vmm_devices::virtio::queue::GuestAccess> =
            Arc::new(RamAccess { ram: ram.clone() });
        let virtio_irq = Arc::new(KvmIrqLine {
            vm: vm.clone(),
            irq: layout::VIRTIO_IRQ_BASE,
        });
        let blk = Arc::new(vmm_devices::virtio::mmio::VirtioBlkMmio::new(
            backend, mem_access, virtio_irq,
        ));
        bus.register_mmio(
            BusRange::new(layout::VIRTIO_MMIO_BASE, layout::VIRTIO_MMIO_SIZE),
            blk as Arc<dyn vmm_devices::device::Device>,
        )?;
        info!(
            base = format_args!("{:#x}", layout::VIRTIO_MMIO_BASE),
            irq = layout::VIRTIO_IRQ_BASE,
            "virtio-blk registered"
        );
    }

    let bus = Arc::new(bus);

    // --- 6. vCPU + long-mode entry ---
    // Place the initial stack HIGH in usable RAM (16-byte aligned), well above
    // the kernel image / GDT / page tables / boot params, so early kernel pushes
    // cannot corrupt them. It grows downward from here. Cap it below the MMIO
    // hole and below the identity-mapped 1 GiB window.
    let ram_top = usable.min(0x4000_0000); // within identity-mapped 1 GiB
    let stack_top = (ram_top - 0x1000) & !0xf;
    let mut vcpu = vm.create_vcpu(0)?;
    vcpu.setup_long_mode(LongModeEntry {
        entry_point: boot_info.entry_point,
        boot_params: boot_info.zero_page,
        stack_top,
        pml4_addr: layout::PML4_START,
        gdt_addr: layout::BOOT_GDT_START,
    })?;

    // Optional watchdog (enable with VMM_DEBUG=1): reports whether the vCPU is
    // making progress. Off by default so it doesn't spam the serial console.
    let exit_counter = Arc::new(std::sync::atomic::AtomicU64::new(0));
    if debug_enabled() {
        let ec = exit_counter.clone();
        let wstop = stop.clone();
        std::thread::Builder::new()
            .name("watchdog".into())
            .spawn(move || {
                let mut last = 0u64;
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    if wstop.load(Ordering::SeqCst) {
                        break;
                    }
                    let now = ec.load(Ordering::Relaxed);
                    if now == last {
                        eprintln!("[watchdog] no vcpu exits in 2s (stuck at {now})");
                    } else {
                        eprintln!("[watchdog] {} exits in last 2s (total {now})", now - last);
                    }
                    last = now;
                }
            })
            .ok();
    }

    // --- 7. Spawn the synchronous vCPU run loop (§1.1: own thread, no await) ---
    let run_stop = stop.clone();
    let run_bus = bus.clone();
    let run_counter = exit_counter.clone();
    // Move `vm` and `ram` into the thread so their KVM fds / mmap outlive the loop.
    std::thread::Builder::new()
        .name("vcpu-0".into())
        .spawn(move || {
            let _keep_vm = vm; // ensure VM fd (and thus vcpu validity) lives here
            let _keep_ram = ram; // ensure guest mmap stays mapped

            if debug_enabled() {
                match vcpu.debug_registers() {
                    Ok(dump) => eprintln!("[boot] vcpu-0 initial state:\n{dump}"),
                    Err(e) => eprintln!("[boot] could not read initial registers: {e}"),
                }
            }

            info!("vcpu-0 entering KVM_RUN loop");
            let mut iter: u64 = 0;
            loop {
                if run_stop.load(Ordering::SeqCst) {
                    info!("vcpu-0 stop requested");
                    break;
                }
                let exit = match vcpu.run() {
                    Ok(e) => e,
                    Err(e) => {
                        if debug_enabled() {
                            if let Ok(d) = vcpu.debug_registers() {
                                eprintln!("[boot] KVM_RUN failed: {e}\nregisters:\n{d}");
                            }
                        }
                        let _ = event_tx.send(VmEvent::Exited(format!("vcpu error: {e}")));
                        break;
                    }
                };

                iter += 1;
                run_counter.fetch_add(1, Ordering::Relaxed);
                if debug_enabled() && iter <= 200 {
                    let is_serial = matches!(
                        &exit,
                        vmm_hypervisor::traits::VcpuExit::IoOut { port, .. }
                            | vmm_hypervisor::traits::VcpuExit::IoIn { port, .. }
                            if (0x3f8..0x400).contains(port)
                    );
                    if !is_serial {
                        eprintln!("[boot] exit #{iter}: {exit:?}");
                    }
                }

                match handle_exit(exit, &run_bus) {
                    Ok(ExitAction::Continue) => {}
                    Ok(ExitAction::ReadCompleted(data)) => {
                        if let Err(e) = vcpu.set_exit_result(&data) {
                            let _ = event_tx.send(VmEvent::Exited(format!("completion: {e}")));
                            break;
                        }
                    }
                    Ok(ExitAction::Shutdown) => {
                        info!("guest requested shutdown");
                        let _ = event_tx.send(VmEvent::Exited("guest shutdown".into()));
                        break;
                    }
                    Err(e) => {
                        // An unhandled/unknown exit (e.g. KVM_EXIT_INTERNAL_ERROR)
                        // must NOT be silently retried — that re-runs the same
                        // faulting instruction forever. Stop and report.
                        if debug_enabled() {
                            if let Ok(d) = vcpu.debug_registers() {
                                eprintln!("[boot] UNHANDLED exit at #{iter}: {e}\nregisters:\n{d}");
                            }
                        }
                        let _ = event_tx.send(VmEvent::Exited(format!("unhandled exit: {e}")));
                        break;
                    }
                }
            }
            info!("vcpu-0 loop ended");
        })
        .expect("spawn vcpu thread");

    Ok(RunningVm {
        uart,
        framebuffer,
        stop,
    })
}

/// Debug output (register dumps, exit traces, watchdog) is gated behind the
/// `VMM_DEBUG` environment variable so normal boots keep the serial console
/// clean for the guest.
fn debug_enabled() -> bool {
    std::env::var_os("VMM_DEBUG").is_some()
}

