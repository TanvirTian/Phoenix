//! The **Device trait** (§3.1).
//!
//! Devices only ever see plain byte slices and offsets relative to their own
//! window — no KVM types, no guest-physical addresses. `&self` (not `&mut self`)
//! because a device is shared as `Arc<dyn Device>` across the Bus and possibly
//! multiple readers; interior mutability (e.g. `Mutex`) lives inside each
//! device as needed.

pub trait Device: Send + Sync {
    /// Handle a guest read of `data.len()` bytes at `offset` within this
    /// device's window. Implementations fill `data`.
    fn read(&self, offset: u64, data: &mut [u8]);

    /// Handle a guest write of `data` at `offset` within this device's window.
    fn write(&self, offset: u64, data: &[u8]);
}
