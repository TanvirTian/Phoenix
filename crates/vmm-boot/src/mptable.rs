//! Intel MultiProcessor (MP) tables (Phase 8 timer fix).
//!
//! Without ACPI or MP tables, a Linux guest cannot discover its local APIC /
//! IO-APIC topology; it falls back to "virtual wire mode" where the local-APIC
//! timer and IO-APIC interrupt routing don't work, so the periodic timer that
//! drives the scheduler and `setitimer`/`select` timeouts never fires reliably.
//! That's what stalls `ping` after the first packet.
//!
//! This module builds a minimal but valid MP configuration describing:
//!   * one processor (BSP) with a local APIC,
//!   * the ISA bus,
//!   * one IO-APIC,
//!   * IO interrupt assignments so ISA IRQs 0..15 route through the IO-APIC.
//!
//! The guest's BIOS-compatibility code scans 0xF0000..0xFFFFF for the MP
//! Floating Pointer ("_MP_"); we place it at [`layout::MPTABLE_START`] with the
//! configuration table immediately after it.

use crate::layout;

/// MP spec structure sizes.
const FLOATING_PTR_LEN: usize = 16;
const CONFIG_HEADER_LEN: usize = 44;
const CPU_ENTRY_LEN: usize = 20;
const BUS_ENTRY_LEN: usize = 8;
const IOAPIC_ENTRY_LEN: usize = 8;
const INTR_ENTRY_LEN: usize = 8;

/// Entry type tags.
const ENTRY_PROCESSOR: u8 = 0;
const ENTRY_BUS: u8 = 1;
const ENTRY_IOAPIC: u8 = 2;
const ENTRY_IOINTR: u8 = 3;
const ENTRY_LOCALINTR: u8 = 4;

/// Interrupt types.
const INT_TYPE_INT: u8 = 0; // vectored interrupt from the IO-APIC
const INT_TYPE_NMI: u8 = 1;
const INT_TYPE_EXTINT: u8 = 3; // 8259-compatible (needed for the PIT on IRQ0)

/// APIC IDs we assign.
const BSP_APIC_ID: u8 = 0;
const IOAPIC_ID: u8 = 2; // distinct from CPU LAPIC IDs

/// checksum: bytes must sum to 0 (mod 256).
fn checksum(bytes: &[u8]) -> u8 {
    let sum = bytes.iter().fold(0u8, |acc, &b| acc.wrapping_add(b));
    (0u8).wrapping_sub(sum)
}

fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    buf[off..off + 2].copy_from_slice(&v.to_le_bytes());
}
fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

/// Build the MP tables for `num_cpus` processors and return the raw bytes to be
/// written at [`layout::MPTABLE_START`] in guest RAM. The floating pointer comes
/// first, then the configuration table.
pub fn build(num_cpus: u8) -> Vec<u8> {
    let num_cpus = num_cpus.max(1);

    // The configuration table starts right after the 16-byte floating pointer.
    let config_phys = layout::MPTABLE_START as u32 + FLOATING_PTR_LEN as u32;

    // --- Configuration table ------------------------------------------------
    let mut cfg = Vec::new();

    // Header (44 bytes). We fill length + checksum after building entries.
    let mut header = vec![0u8; CONFIG_HEADER_LEN];
    header[0..4].copy_from_slice(b"PCMP"); // signature
    // base_table_length @4 (u16), spec_rev @6 (u8), checksum @7 (u8) — later.
    header[6] = 4; // MP spec 1.4
    // OEM id @8 (8 bytes), product id @16 (12 bytes) — leave spaces.
    header[8..16].copy_from_slice(b"PHOENIX ");
    header[16..28].copy_from_slice(b"VMM 0.1     ");
    // oem_table_ptr @28 (u32)=0, oem_table_size @32 (u16)=0.
    // entry_count @34 (u16) — later.
    put_u32(&mut header, 36, layout::APIC_DEFAULT_PHYS); // local APIC address
    // ext_table_length @40 (u16)=0, ext_table_checksum @42 (u8)=0, reserved @43.

    let mut entries = Vec::new();
    let mut entry_count: u16 = 0;

    // Processor entries (one per CPU). BSP is the first, flagged enabled+BSP.
    for i in 0..num_cpus {
        let mut e = vec![0u8; CPU_ENTRY_LEN];
        e[0] = ENTRY_PROCESSOR;
        e[1] = BSP_APIC_ID + i; // local APIC id
        e[2] = 0x14; // local APIC version (integrated APIC)
        // cpu_flags @3: bit0 = enabled, bit1 = BSP (only CPU 0).
        e[3] = if i == 0 { 0b11 } else { 0b01 };
        // cpu_signature @4 (u32), feature_flags @8 (u32) — a plausible value.
        put_u32(&mut e, 4, 0x600); // family 6
        put_u32(&mut e, 8, 0x201); // FPU + APIC feature bits
        entries.extend_from_slice(&e);
        entry_count += 1;
    }

    // Bus entry: the ISA bus (id 0).
    {
        let mut e = vec![0u8; BUS_ENTRY_LEN];
        e[0] = ENTRY_BUS;
        e[1] = 0; // bus id
        e[2..8].copy_from_slice(b"ISA   ");
        entries.extend_from_slice(&e);
        entry_count += 1;
    }

    // IO-APIC entry.
    {
        let mut e = vec![0u8; IOAPIC_ENTRY_LEN];
        e[0] = ENTRY_IOAPIC;
        e[1] = IOAPIC_ID;
        e[2] = 0x20; // IO-APIC version
        e[3] = 1; // flags: enabled
        put_u32(&mut e, 4, layout::IOAPIC_DEFAULT_PHYS);
        entries.extend_from_slice(&e);
        entry_count += 1;
    }

    // IO interrupt assignments: route ISA IRQ 0..15 -> IO-APIC input 0..15.
    // IRQ0 (the PIT) is delivered as ExtINT so the 8259-timer path works, and
    // also as a normal INT on IO-APIC line 2 (the classic PC "timer on pin 2"
    // wiring the kernel expects). The rest map 1:1.
    for irq in 0u8..16 {
        let mut e = vec![0u8; INTR_ENTRY_LEN];
        e[0] = ENTRY_IOINTR;
        // IRQ0 special-case: ExtINT to IO-APIC pin 0 AND INT to pin 2.
        if irq == 0 {
            // IRQ0/timer. KVM's in-kernel irqchip installs a default GSI
            // routing (GSI0 -> IOAPIC pin 0 AND PIC pin 0). Describe IRQ0 as a
            // plain vectored INT on IO-APIC pin 0 so check_timer()'s direct
            // route succeeds and matches KVM's default routing. We deliberately
            // do NOT advertise an ExtINT entry here: KVM doesn't wire an
            // 8259-style ExtINT source, and claiming one triggers the guest's
            // "ExtINT not setup in hardware but reported by MP table" path and
            // leaves the timer mis-programmed.
            e[1] = INT_TYPE_INT;
            put_u16(&mut e, 2, 0); // conforming polarity/trigger
            e[4] = 0; // ISA bus
            e[5] = 0; // source IRQ 0
            e[6] = IOAPIC_ID;
            e[7] = 0; // dest IO-APIC pin 0
            entries.extend_from_slice(&e);
            entry_count += 1;

            // Also present it on pin 2 (the classic PC "timer on pin 2" wiring
            // that check_timer() falls back to), as a plain INT.
            let mut e2 = vec![0u8; INTR_ENTRY_LEN];
            e2[0] = ENTRY_IOINTR;
            e2[1] = INT_TYPE_INT;
            put_u16(&mut e2, 2, 0);
            e2[4] = 0;
            e2[5] = 0;
            e2[6] = IOAPIC_ID;
            e2[7] = 2; // dest pin 2
            entries.extend_from_slice(&e2);
            entry_count += 1;
            continue;
        }
        e[1] = INT_TYPE_INT;
        // virtio-mmio devices (IRQ_BASE .. IRQ_BASE+VIRTIO_MMIO_COUNT) are
        // LEVEL-triggered, active-high — matching the Linux `virtio_mmio`
        // driver's INTERRUPT_STATUS / INTERRUPT_ACK model (the line stays
        // asserted until the driver acks it). Declaring them edge-triggered
        // (flags=0, ISA-conforming) forces a manual edge pulse and hits the
        // IOAPIC remote-IRR gate, which drops interrupts under light traffic.
        // Everything else (e.g. COM1) stays ISA-conforming (edge), which is
        // correct for a genuine ISA source.
        // MP-spec INTR flags: bits[1:0] polarity (01=active-high),
        // bits[3:2] trigger (11=level) => 0b1101.
        let is_virtio = (crate::layout::VIRTIO_IRQ_BASE
            ..crate::layout::VIRTIO_IRQ_BASE + crate::layout::VIRTIO_MMIO_COUNT as u32)
            .contains(&(irq as u32));
        let flags: u16 = if is_virtio { 0b1101 } else { 0 };
        put_u16(&mut e, 2, flags);
        e[4] = 0; // ISA bus
        e[5] = irq; // source bus IRQ
        e[6] = IOAPIC_ID;
        e[7] = irq; // dest IO-APIC pin == IRQ (identity for the rest)
        entries.extend_from_slice(&e);
        entry_count += 1;
    }

    // Local interrupt assignments: LINT0 = ExtINT, LINT1 = NMI (standard).
    {
        let mut e = vec![0u8; INTR_ENTRY_LEN];
        e[0] = ENTRY_LOCALINTR;
        e[1] = INT_TYPE_EXTINT;
        put_u16(&mut e, 2, 0);
        e[4] = 0;
        e[5] = 0;
        e[6] = 0xff; // all CPUs
        e[7] = 0; // LINT0
        entries.extend_from_slice(&e);
        entry_count += 1;

        let mut e2 = vec![0u8; INTR_ENTRY_LEN];
        e2[0] = ENTRY_LOCALINTR;
        e2[1] = INT_TYPE_NMI;
        put_u16(&mut e2, 2, 0);
        e2[4] = 0;
        e2[5] = 0;
        e2[6] = 0xff;
        e2[7] = 1; // LINT1
        entries.extend_from_slice(&e2);
        entry_count += 1;
    }

    // Finalize header: base table length + entry count + checksum.
    let base_len = (CONFIG_HEADER_LEN + entries.len()) as u16;
    put_u16(&mut header, 4, base_len);
    put_u16(&mut header, 34, entry_count);
    // checksum over header+entries must be zero.
    cfg.extend_from_slice(&header);
    cfg.extend_from_slice(&entries);
    let cksum = checksum(&cfg);
    cfg[7] = cksum;

    // --- Floating pointer (16 bytes) ---------------------------------------
    let mut fptr = vec![0u8; FLOATING_PTR_LEN];
    fptr[0..4].copy_from_slice(b"_MP_");
    put_u32(&mut fptr, 4, config_phys); // physical addr of the config table
    fptr[8] = 1; // length in 16-byte paragraphs
    fptr[9] = 4; // MP spec revision 1.4
    // fptr[10] = checksum (later); fptr[11..16] = feature bytes (0 => use table).
    let fp_cksum = checksum(&fptr);
    fptr[10] = fp_cksum;

    // Concatenate: floating pointer, then config table.
    let mut out = Vec::with_capacity(FLOATING_PTR_LEN + cfg.len());
    out.extend_from_slice(&fptr);
    out.extend_from_slice(&cfg);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floating_pointer_signature_and_checksum() {
        let t = build(1);
        assert_eq!(&t[0..4], b"_MP_");
        // Floating pointer checksum: first 16 bytes sum to 0.
        let sum = t[0..FLOATING_PTR_LEN].iter().fold(0u8, |a, &b| a.wrapping_add(b));
        assert_eq!(sum, 0);
    }

    #[test]
    fn config_table_signature_and_checksum() {
        let t = build(1);
        let cfg = &t[FLOATING_PTR_LEN..];
        assert_eq!(&cfg[0..4], b"PCMP");
        // base_table_length covers header + entries.
        let base_len = u16::from_le_bytes([cfg[4], cfg[5]]) as usize;
        let sum = cfg[..base_len].iter().fold(0u8, |a, &b| a.wrapping_add(b));
        assert_eq!(sum, 0, "config table checksum must be zero");
    }

    #[test]
    fn scales_with_cpu_count() {
        let one = build(1);
        let four = build(4);
        assert!(four.len() > one.len(), "more CPUs => more processor entries");
    }
}
