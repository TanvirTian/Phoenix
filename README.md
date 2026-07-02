# Phoenix
Phoenix is a KVM-based Type-2 Virtual Machine Monitor written in Rust, with a PyQt6 frontend.

This repository is my journey of learning virtualization by building a hypervisor from scratch.

Drives KVM directly through Linux ioctls and implements its own virtual devices not a QEMU wrapper.

[![Core](https://img.shields.io/badge/core-Rust-orange)](#) [![Frontend](https://img.shields.io/badge/frontend-PyQt6-blue)](#) [![Virtualization](https://img.shields.io/badge/virtualization-KVM-green)](#) [![Platform](https://img.shields.io/badge/platform-Linux-lightgrey)](#) [![Clippy](https://img.shields.io/badge/clippy-0%20warnings-brightgreen)](#)

Boots real Linux kernels into an interactive shell, mounts VirtIO block devices, and renders a live guest framebuffer through a custom GUI.


## Table of contents

-   [What this is](#what-this-is)
-   [Architecture](#architecture-two-inviolable-boundaries)
    -   [Engineering conventions](#engineering-conventions)
-   [Workspace layout](#workspace-layout)
-   [Quick start](#quick-start)
-   [Implemented components](#implemented-components)
-   [Demos](#demos)
    -   [Running Linux](#running-linux)
    -   [VirtIO block storage](#virtio-block-storage)
    -   [Zero-copy framebuffer](#zero-copy-framebuffer)
-   [Testing](#testing)
-   [Preparing a guest kernel & disk](#preparing-a-guest-kernel--disk)

----------

## What this is

`vmm` is a complete, working hypervisor built from the ground up. It opens `/dev/kvm`, sets up guest memory and a virtual CPU, enters the guest in 64-bit long mode, and handles every VM exit itself. All device emulation — the serial port, the VirtIO block device, the framebuffer — is **our own code**, not delegated to QEMU or any other VMM.

A running VM gives you:

🐧 **Real Linux**

Direct-kernel boot of a `bzImage` to a fully interactive BusyBox shell over an emulated 16550 UART (keystrokes echo and execute).

💾 **VirtIO block storage**

The guest's real `virtio_blk` driver probes our MMIO transport and gets a `/dev/vda` it can `mkfs`/`mount`.

🖥️ **A live framebuffer**

Zero-copy shared memory renders guest pixels in the GUI at ~30 FPS (`cat /dev/urandom > /dev/fb0` fills the window with noise).

🎛️ **A polished GUI**

A PyQt6 desktop app with a colored, ANSI-aware serial console, VM configuration, and a display tab.


## Project layout
```
Phoenix
├── Cargo.lock
├── Cargo.toml
├── crates
│   ├── vmm-boot
│   │   ├── Cargo.toml
│   │   └── src
│   │       ├── layout.rs
│   │       ├── lib.rs
│   │       └── linux.rs
│   ├── vmm-daemon
│   │   ├── Cargo.toml
│   │   └── src
│   │       ├── bin
│   │       │   └── boot_kernel.rs
│   │       ├── control
│   │       │   ├── manager.rs
│   │       │   ├── mod.rs
│   │       │   ├── protocol.rs
│   │       │   └── server.rs
│   │       ├── lib.rs
│   │       ├── main.rs
│   │       └── vm
│   │           ├── boot.rs
│   │           ├── exit.rs
│   │           ├── framebuffer.rs
│   │           ├── mod.rs
│   │           ├── state.rs
│   │           └── vm.rs
│   ├── vmm-devices
│   │   ├── Cargo.toml
│   │   └── src
│   │       ├── bus.rs
│   │       ├── device.rs
│   │       ├── fb.rs
│   │       ├── lib.rs
│   │       ├── pci_stub.rs
│   │       ├── rtc_cmos.rs
│   │       ├── uart.rs
│   │       └── virtio
│   │           ├── block.rs
│   │           ├── mmio.rs
│   │           ├── mod.rs
│   │           ├── net.rs
│   │           └── queue.rs
│   └── vmm-hypervisor
│       ├── Cargo.toml
│       └── src
│           ├── bin
│           │   └── hlt_test.rs
│           ├── kvm
│           │   ├── memory.rs
│           │   ├── mod.rs
│           │   ├── vcpu_fd.rs
│           │   └── vm_fd.rs
│           ├── lib.rs
│           └── traits.rs
├── frontend
│   └── src
│       ├── client.py
│       ├── main.py
│       └── views
│           ├── __init__.py
│           ├── main_window.py
│           ├── serial_console.py
│           └── vm_display.py
└── README.md

```

## Quick start

> **Requirements:** Linux host with `/dev/kvm` (hardware virtualization enabled), a recent Rust toolchain (`cargo`), Python 3 with PyQt6 for the GUI, and a `bzImage` guest kernel that speaks `ttyS0` (see [Preparing a guest kernel & disk](#preparing-a-guest-kernel--disk)).

### 1. Build and test the core

```bash
cd vmm
cargo build                   # build the whole workspace
cargo test                    # 41 unit tests: all pass without /dev/kvm
cargo clippy --all-targets    # zero warnings

```

### 2. Boot Linux (standalone tool — simplest first boot)

```bash
cargo run --bin boot-kernel -- <bzImage> 512 "console=ttyS0 reboot=k panic=1 pci=off"
# guest serial output prints to stdout; type into the shell; Ctrl-C to stop

```

With a disk and initramfs, and the VirtIO device advertised on the cmdline:

```bash
DISK=~/disk.img INITRD=~/initramfs.cpio.gz \
  cargo run --bin boot-kernel -- ~/tiny-bzImage 512 \
  "console=ttyS0 virtio_mmio.device=0x1000@0xfe000000:5"

```

### 3. Run the full GUI (daemon + PyQt6 frontend)

In one terminal, start the daemon (it owns `/dev/kvm`):

```bash
cargo run --bin vmm-daemon -- --socket /tmp/vmm.sock

```

In another terminal, launch the frontend:

```bash
python3 frontend/src/main.py --socket /tmp/vmm.sock

```

Fill in the kernel/disk/initrd, optionally tick **Display (1024×768)**, click **▶ Start**, use the **Serial Console** tab, and click **🖥 Attach Display** to see the guest framebuffer.

----------

## Implemented components

The hypervisor is built as a collection of independent subsystems. Every major component has been implemented and verified with a working guest.

Component

Description

Verification

**KVM hypervisor**

Safe wrapper around the Linux KVM API. Creates VMs, allocates guest memory, manages vCPUs, and implements the VM-exit completion contract.

Executes `KVM_RUN` and correctly handles `VcpuExit::Hlt`

**Boot pipeline**

Loads a Linux `bzImage`, configures boot parameters, enters 64-bit long mode, and boots an unmodified Linux kernel.

Linux reaches an interactive BusyBox shell

**Device bus & UART**

Generic MMIO/PIO device bus with an emulated 16550 UART for serial I/O.

Full bidirectional serial console

**Control plane**

Tokio-based daemon exposing a Unix Domain Socket API with JSON messages and asynchronous event streaming.

External clients create and control VMs

**VirtIO block device**

VirtIO-MMIO transport, virtqueue implementation, and file-backed block device.

Guest detects `/dev/vda`, mounts and accesses the filesystem

**Desktop frontend**

PyQt6 GUI with an ANSI-aware terminal, VM controls, and configuration panel.

Interactive Linux console inside the GUI

**Framebuffer device**

Zero-copy framebuffer using `memfd` and `SCM_RIGHTS` file descriptor passing.

Guest-generated pixels render live in the display window

----------

## Demos

### Running Linux

Phoenix boots an unmodified Linux kernel directly through KVM and provides an interactive serial console.

```
=== Welcome to Phoenix ===
~ # ls
bin  dev  init  proc  root  sys
~ # uname
Linux

```

### VirtIO block storage

The guest kernel uses its native VirtIO block driver to communicate with our emulated VirtIO-MMIO device. The hypervisor implements the MMIO transport, virtqueue parsing, descriptor walking, and a file-backed virtual disk, allowing the guest to detect, mount, and access a virtual block device.

**Verified guest output:**

```text
virtio-mmio: Registering device virtio-mmio.0 at 0xfe000000-0xfe000fff, IRQ 5.
virtio_blk virtio0: [vda] 32768 512-byte logical blocks (16.8 MB / 16.0 MiB)

~ # ls /dev/vda
/dev/vda

```

### Zero-copy framebuffer

Phoenix implements a shared-memory framebuffer that avoids copying pixel data between the guest and the GUI.

At VM startup, the daemon creates a `memfd`, maps it into its own address space, and exposes the same memory to the guest as a framebuffer. The file descriptor is transferred to the PyQt6 frontend using `SCM_RIGHTS`, allowing both the guest and the GUI to access the identical memory pages.

----------

## Testing

The project currently contains **41 automated unit tests** covering device emulation, bus routing, protocol serialization, VirtIO queue logic, and other core subsystems.

The tests require **no hardware virtualization**, **no `/dev/kvm`**, and can run on any standard Linux environment.

```bash
cargo test

```

----------

## Preparing a guest kernel & disk

Phoenix boots standard Linux kernels via direct kernel boot. To use the VirtIO block device and framebuffer, build a kernel with the required drivers enabled.

### 1. Build the guest kernel

```bash
./scripts/config \
  --enable VIRTIO \
  --enable VIRTIO_MMIO \
  --enable VIRTIO_MMIO_CMDLINE_DEVICES \
  --enable VIRTIO_BLK \
  --enable BLOCK \
  --enable EXT4_FS \
  --enable FB \
  --enable FB_SIMPLE \
  --enable SYSFB_SIMPLEFB

make olddefconfig
make -j"$(nproc)" bzImage

```

### 2. Create a virtual disk

```bash
dd if=/dev/zero of=~/disk.img bs=1M count=16
mkfs.ext4 ~/disk.img

```

### 3. Boot the guest

Expose the VirtIO block device by passing the MMIO device description on the kernel command line:

```text
virtio_mmio.device=0x1000@0xfe000000:5

```

Once Linux has booted, the virtual disk appears as `/dev/vda`:

```bash
mkdir -p /mnt
mount -t ext4 /dev/vda /mnt
ls /mnt

```
