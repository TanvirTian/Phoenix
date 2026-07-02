//! virtio-blk backend (Phase 4, §4).
//!
//! Scaffold: models the virtio-blk request header and a file-backed store.
//! The MMIO transport register handling + virtqueue processing is wired in
//! Phase 4; the disk backend and request codec are defined here so they can be
//! unit-tested independently of KVM.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Mutex;

pub const VIRTIO_BLK_T_IN: u32 = 0; // read
pub const VIRTIO_BLK_T_OUT: u32 = 1; // write
pub const VIRTIO_BLK_T_FLUSH: u32 = 4;

pub const VIRTIO_BLK_S_OK: u8 = 0;
pub const VIRTIO_BLK_S_IOERR: u8 = 1;
pub const VIRTIO_BLK_S_UNSUPP: u8 = 2;

pub const SECTOR_SIZE: u64 = 512;

/// 16-byte virtio-blk request header (little-endian on the wire).
#[derive(Debug, Clone, Copy)]
pub struct BlkReqHeader {
    pub req_type: u32,
    pub reserved: u32,
    pub sector: u64,
}

impl BlkReqHeader {
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 16 {
            return None;
        }
        Some(Self {
            req_type: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            reserved: u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            sector: u64::from_le_bytes(bytes[8..16].try_into().unwrap()),
        })
    }
}

#[derive(thiserror::Error, Debug)]
pub enum BlockError {
    #[error("disk I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// A raw-file-backed block device.
pub struct BlockBackend {
    file: Mutex<File>,
    capacity_sectors: u64,
}

impl BlockBackend {
    pub fn open(path: &str) -> Result<Self, BlockError> {
        let file = File::options().read(true).write(true).open(path)?;
        let len = file.metadata()?.len();
        Ok(Self {
            file: Mutex::new(file),
            capacity_sectors: len / SECTOR_SIZE,
        })
    }

    pub fn capacity_sectors(&self) -> u64 {
        self.capacity_sectors
    }

    pub fn read_sectors(&self, sector: u64, buf: &mut [u8]) -> Result<(), BlockError> {
        let mut f = self.file.lock().unwrap();
        f.seek(SeekFrom::Start(sector * SECTOR_SIZE))?;
        f.read_exact(buf)?;
        Ok(())
    }

    pub fn write_sectors(&self, sector: u64, buf: &[u8]) -> Result<(), BlockError> {
        let mut f = self.file.lock().unwrap();
        f.seek(SeekFrom::Start(sector * SECTOR_SIZE))?;
        f.write_all(buf)?;
        Ok(())
    }

    pub fn flush(&self) -> Result<(), BlockError> {
        self.file.lock().unwrap().flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrips() {
        let mut b = Vec::new();
        b.extend_from_slice(&VIRTIO_BLK_T_OUT.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&42u64.to_le_bytes());
        let h = BlkReqHeader::parse(&b).unwrap();
        assert_eq!(h.req_type, VIRTIO_BLK_T_OUT);
        assert_eq!(h.sector, 42);
    }

    #[test]
    fn file_backend_read_write() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("vmm-blk-test-{}.img", std::process::id()));
        {
            let mut f = File::create(&path).unwrap();
            f.write_all(&[0u8; (SECTOR_SIZE * 4) as usize]).unwrap();
        }
        let be = BlockBackend::open(path.to_str().unwrap()).unwrap();
        assert_eq!(be.capacity_sectors(), 4);
        be.write_sectors(1, &[0xAB; SECTOR_SIZE as usize]).unwrap();
        let mut buf = [0u8; SECTOR_SIZE as usize];
        be.read_sectors(1, &mut buf).unwrap();
        assert_eq!(buf, [0xAB; SECTOR_SIZE as usize]);
        std::fs::remove_file(&path).ok();
    }
}
