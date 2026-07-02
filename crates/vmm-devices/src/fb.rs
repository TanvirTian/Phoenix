//! Shared-memory framebuffer (Phase 6, §3.4 / §4).
//!
//! The daemon creates a `memfd`, maps it both into the guest physical address
//! space (as the framebuffer aperture) and passes the FD to PyQt over UDS via
//! `SCM_RIGHTS`. This module models the framebuffer *geometry* and the Device
//! shim so guest MMIO writes land in the shared mapping. The actual memfd
//! creation + FD passing is a daemon (syscall) concern and lives there.

use std::sync::atomic::{AtomicU64, Ordering};

/// Bytes per pixel (XRGB8888).
pub const BYTES_PER_PIXEL: usize = 4;

#[derive(Debug, Clone, Copy)]
pub struct FbGeometry {
    pub width: u32,
    pub height: u32,
}

impl FbGeometry {
    pub fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// Total framebuffer size in bytes.
    pub fn size_bytes(&self) -> usize {
        self.width as usize * self.height as usize * BYTES_PER_PIXEL
    }
}

/// Metadata about the shared framebuffer, shareable across threads. The pixel
/// bytes themselves live in the memfd mapping owned by the daemon; this struct
/// tracks geometry and a frame counter the GUI can poll for change detection.
pub struct Framebuffer {
    pub geometry: FbGeometry,
    frame_seq: AtomicU64,
}

impl Framebuffer {
    pub fn new(geometry: FbGeometry) -> Self {
        Self {
            geometry,
            frame_seq: AtomicU64::new(0),
        }
    }

    /// Bump the frame sequence (called when the guest touches the aperture).
    pub fn mark_dirty(&self) {
        self.frame_seq.fetch_add(1, Ordering::Relaxed);
    }

    pub fn frame_seq(&self) -> u64 {
        self.frame_seq.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_computation() {
        let g = FbGeometry::new(640, 480);
        assert_eq!(g.size_bytes(), 640 * 480 * 4);
    }

    #[test]
    fn dirty_bumps_seq() {
        let fb = Framebuffer::new(FbGeometry::new(1, 1));
        assert_eq!(fb.frame_seq(), 0);
        fb.mark_dirty();
        assert_eq!(fb.frame_seq(), 1);
    }
}
