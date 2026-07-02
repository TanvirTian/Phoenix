//! virtio-net backend (Phase 4+, §4). Scaffold.
//!
//! Defines the virtio-net header and a tap-like byte sink/source abstraction.
//! Full RX/TX virtqueue processing is wired alongside virtio-blk.

/// 12-byte (legacy) virtio-net header prepended to each packet.
pub const VIRTIO_NET_HDR_LEN: usize = 12;

#[derive(Debug, Clone, Copy, Default)]
pub struct VirtioNetHdr {
    pub flags: u8,
    pub gso_type: u8,
    pub hdr_len: u16,
    pub gso_size: u16,
    pub csum_start: u16,
    pub csum_offset: u16,
}

impl VirtioNetHdr {
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < VIRTIO_NET_HDR_LEN {
            return None;
        }
        Some(Self {
            flags: bytes[0],
            gso_type: bytes[1],
            hdr_len: u16::from_le_bytes([bytes[2], bytes[3]]),
            gso_size: u16::from_le_bytes([bytes[4], bytes[5]]),
            csum_start: u16::from_le_bytes([bytes[6], bytes[7]]),
            csum_offset: u16::from_le_bytes([bytes[8], bytes[9]]),
        })
    }
}

/// Host-side network backend (e.g. a tap device). Injected by the daemon.
pub trait NetBackend: Send + Sync {
    /// Send a frame produced by the guest onto the host network.
    fn transmit(&self, frame: &[u8]) -> std::io::Result<()>;
    /// Poll for a frame from the host to hand to the guest, if any.
    fn receive(&self) -> std::io::Result<Option<Vec<u8>>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_parse() {
        let bytes = [0u8; VIRTIO_NET_HDR_LEN];
        assert!(VirtioNetHdr::parse(&bytes).is_some());
        assert!(VirtioNetHdr::parse(&bytes[..4]).is_none());
    }
}
