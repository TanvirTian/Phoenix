//! Host TAP-device backend for virtio-net (Phase 8).
//!
//! Opens `/dev/net/tun`, attaches to an existing TAP interface (created by the
//! host, e.g. `ip tuntap add tap0 mode tap user <you>`), and implements the
//! KVM-agnostic [`NetBackend`](vmm_devices::virtio::net::NetBackend) trait so
//! the virtio-net device can move Ethernet frames between the guest and the host
//! network stack.
//!
//! This is a *daemon* concern (it does raw syscalls), keeping `vmm-devices`
//! pure — exactly like `KvmIrqLine` and `RamAccess` in `boot.rs`.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use vmm_devices::virtio::net::NetBackend;

// --- ioctl / flag constants (from <linux/if_tun.h>, <net/if.h>) ---

const IFF_TAP: i16 = 0x0002;
const IFF_NO_PI: i16 = 0x1000;
const IFNAMSIZ: usize = 16;

// TUNSETIFF = _IOW('T', 202, int)
const TUNSETIFF: libc::c_ulong = 0x4004_54ca;

/// `struct ifreq` (the subset we need): name + flags in a fixed 40-byte layout.
#[repr(C)]
struct IfReq {
    ifr_name: [libc::c_char; IFNAMSIZ],
    ifr_flags: i16,
    _pad: [u8; 22],
}

/// A TAP device opened and attached to interface `name`.
pub struct TapBackend {
    fd: OwnedFd,
}

impl TapBackend {
    /// Open `/dev/net/tun` and attach to the (already-created) TAP `name`.
    pub fn open(name: &str) -> io::Result<Self> {
        if name.len() >= IFNAMSIZ {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "tap interface name too long",
            ));
        }

        // Open the clone device.
        let raw = unsafe {
            libc::open(
                c"/dev/net/tun".as_ptr(),
                libc::O_RDWR | libc::O_NONBLOCK,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a valid, just-opened fd we now own.
        let fd = unsafe { OwnedFd::from_raw_fd_checked(raw)? };

        // Build the ifreq and issue TUNSETIFF to bind to `name` in TAP mode.
        let mut req = IfReq {
            ifr_name: [0; IFNAMSIZ],
            ifr_flags: IFF_TAP | IFF_NO_PI,
            _pad: [0; 22],
        };
        for (i, b) in name.as_bytes().iter().enumerate() {
            req.ifr_name[i] = *b as libc::c_char;
        }

        // SAFETY: fd is valid; req matches the kernel's struct ifreq layout.
        let rc = unsafe { libc::ioctl(fd.as_raw_fd(), TUNSETIFF, &mut req as *mut _) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self { fd })
    }

    fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

impl NetBackend for TapBackend {
    /// Write a guest-produced Ethernet frame to the TAP fd (host receives it).
    fn transmit(&self, frame: &[u8]) -> io::Result<()> {
        // SAFETY: fd valid; writing `frame.len()` bytes from a valid slice.
        let n = unsafe {
            libc::write(self.raw(), frame.as_ptr() as *const libc::c_void, frame.len())
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            // A full TAP queue (EAGAIN) is not fatal — drop the frame.
            if e.raw_os_error() == Some(libc::EAGAIN) {
                return Ok(());
            }
            return Err(e);
        }
        Ok(())
    }

    /// Non-blocking read of one frame from the TAP fd (guest will receive it).
    fn receive(&self) -> io::Result<Option<Vec<u8>>> {
        let mut buf = vec![0u8; 2048];
        // SAFETY: fd valid; reading into a valid, sized buffer.
        let n = unsafe {
            libc::read(self.raw(), buf.as_mut_ptr() as *mut libc::c_void, buf.len())
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EAGAIN) {
                return Ok(None); // nothing available right now
            }
            return Err(e);
        }
        if n == 0 {
            return Ok(None);
        }
        buf.truncate(n as usize);
        Ok(Some(buf))
    }
}

/// Small extension: build an `OwnedFd` from a raw fd, erroring on -1.
trait OwnedFdExt: Sized {
    unsafe fn from_raw_fd_checked(fd: RawFd) -> io::Result<OwnedFd>;
}
impl OwnedFdExt for OwnedFd {
    unsafe fn from_raw_fd_checked(fd: RawFd) -> io::Result<OwnedFd> {
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        use std::os::fd::FromRawFd;
        Ok(OwnedFd::from_raw_fd(fd))
    }
}
