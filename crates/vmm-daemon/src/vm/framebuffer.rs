//! Shared-memory framebuffer plumbing (Phase 6, §3.4).
//!
//! Creates a `memfd`, sizes it, and `mmap`s it into the daemon. The same host
//! mapping is then registered as guest RAM (a KVM memslot) at the framebuffer
//! aperture, so guest pixel writes land directly in the memfd — zero copy, no
//! MMIO exits. The raw FD is handed to the GUI over the control socket via
//! `SCM_RIGHTS`, and the GUI mmaps the *same* pages, so guest-drawn pixels
//! appear in the window instantly.

use std::ffi::CString;
use std::num::NonZeroUsize;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use nix::sys::mman::{mmap, MapFlags, ProtFlags};
use nix::sys::memfd::{memfd_create, MemFdCreateFlag};
use nix::unistd::ftruncate;

use vmm_devices::fb::FbGeometry;

#[derive(thiserror::Error, Debug)]
pub enum FbError {
    #[error("memfd/mmap syscall failed: {0}")]
    Syscall(#[from] nix::errno::Errno),
    #[error("invalid framebuffer size")]
    BadSize,
}

/// Owns the memfd and its host mapping for the lifetime of the VM.
pub struct SharedFramebuffer {
    pub geometry: FbGeometry,
    pub size: usize,
    /// Host virtual address of the mapping (registered as guest RAM).
    pub host_addr: u64,
    /// The memfd; kept open so the mapping stays valid and so we can pass a dup
    /// to the GUI. Also drop-closes it on VM teardown.
    fd: OwnedFd,
}

impl SharedFramebuffer {
    /// Create a memfd of `geometry` and map it read/write into the daemon.
    pub fn new(geometry: FbGeometry) -> Result<Self, FbError> {
        let size = geometry.size_bytes();
        let len = NonZeroUsize::new(size).ok_or(FbError::BadSize)?;

        let name = CString::new("vmm-framebuffer").unwrap();
        let fd = memfd_create(&name, MemFdCreateFlag::MFD_CLOEXEC)?;
        ftruncate(&fd, size as i64)?;

        // SAFETY: fresh memfd sized to `size`; MAP_SHARED so the guest, the
        // daemon, and (via the passed FD) the GUI all see the same pages.
        let ptr = unsafe {
            mmap(
                None,
                len,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                &fd,
                0,
            )?
        };

        Ok(Self {
            geometry,
            size,
            host_addr: ptr.as_ptr() as u64,
            fd,
        })
    }

    /// The raw fd to send to the GUI over SCM_RIGHTS. The caller must not close
    /// it (this struct owns it); `sendmsg` only duplicates it into the peer.
    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}
