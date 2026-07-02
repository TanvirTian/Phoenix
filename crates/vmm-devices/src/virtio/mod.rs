//! virtio device family (Phase 4, §4).
//!
//! * [`queue`] — split-virtqueue parsing (the hardest part).
//! * [`block`] — virtio-blk file backend.
//! * [`net`] — virtio-net backend.
//!
//! The virtio-mmio *transport* register model (magic value, device/driver
//! feature bits, queue selection, notify) is shared by all device types; it is
//! introduced in Phase 4 and lives here.

pub mod block;
pub mod mmio;
pub mod net;
pub mod queue;

/// virtio-mmio "magic value" register content: little-endian "virt".
pub const VIRTIO_MMIO_MAGIC: u32 = 0x7472_6976; // "virt"
/// virtio version 2 (modern / VIRTIO 1.x).
pub const VIRTIO_MMIO_VERSION: u32 = 2;

/// Device type IDs (subset).
pub const VIRTIO_ID_NET: u32 = 1;
pub const VIRTIO_ID_BLOCK: u32 = 2;

/// Common virtio-mmio register offsets (subset used in Phase 4).
pub mod mmio_regs {
    pub const MAGIC_VALUE: u64 = 0x000;
    pub const VERSION: u64 = 0x004;
    pub const DEVICE_ID: u64 = 0x008;
    pub const VENDOR_ID: u64 = 0x00c;
    pub const DEVICE_FEATURES: u64 = 0x010;
    pub const DEVICE_FEATURES_SEL: u64 = 0x014;
    pub const DRIVER_FEATURES: u64 = 0x020;
    pub const DRIVER_FEATURES_SEL: u64 = 0x024;
    pub const QUEUE_SEL: u64 = 0x030;
    pub const QUEUE_NUM_MAX: u64 = 0x034;
    pub const QUEUE_NUM: u64 = 0x038;
    pub const QUEUE_READY: u64 = 0x044;
    pub const QUEUE_NOTIFY: u64 = 0x050;
    pub const INTERRUPT_STATUS: u64 = 0x060;
    pub const INTERRUPT_ACK: u64 = 0x064;
    pub const STATUS: u64 = 0x070;
    pub const QUEUE_DESC_LOW: u64 = 0x080;
    pub const QUEUE_DESC_HIGH: u64 = 0x084;
    pub const QUEUE_AVAIL_LOW: u64 = 0x090;
    pub const QUEUE_AVAIL_HIGH: u64 = 0x094;
    pub const QUEUE_USED_LOW: u64 = 0x0a0;
    pub const QUEUE_USED_HIGH: u64 = 0x0a4;
    pub const CONFIG: u64 = 0x100;
}
