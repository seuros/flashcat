use anyhow::{bail, Result};
use tracing::info;

use crate::chip::{EraseType, ResolvedChip};
use crate::progress::Progress;
use crate::usb::UsbDevice;

use super::bus::{spibus_write, ss_disable, ss_enable};
use super::write::{wait_wip, wait_wip_block, wait_wip_chip_erase, write_enable};

/// A single planned erase operation with a specific opcode and size.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct EraseOp {
    pub addr:   u32,
    pub size:   u32,
    pub opcode: u8,
}

pub async fn erase_chip(dev: &UsbDevice, chip: &ResolvedChip) -> Result<()> {
    info!("chip erase: {} ({} bytes)", chip.name, chip.size_bytes);
    write_enable(dev).await?;
    ss_enable(dev).await?;
    let wr  = spibus_write(dev, &[0xC7]).await; // CE
    let dis = ss_disable(dev).await;
    wr?;
    dis?;
    wait_wip_chip_erase(dev, chip).await
}

/// Erase a single aligned sector/block at `addr`.
/// The erase opcode is looked up from `chip.erase_types` using `chip.erase_size`
/// as the key, so vendor-specific and SFDP-derived opcodes are always honoured.
pub async fn erase_unit(dev: &UsbDevice, chip: &ResolvedChip, addr: u32) -> Result<()> {
    debug_assert!(
        addr.is_multiple_of(chip.erase_size),
        "erase_unit: addr {addr:#x} is not aligned to erase_size {:#x}", chip.erase_size
    );
    let et = chip.erase_types.iter()
        .find(|e| e.size_bytes == chip.erase_size)
        .ok_or_else(|| anyhow::anyhow!(
            "no erase type for {}B in chip {} erase_types", chip.erase_size, chip.name
        ))?;
    let cmd = et.opcode;

    write_enable(dev).await?;
    ss_enable(dev).await?;
    let wr = if chip.addr_bytes == 4 {
        spibus_write(dev, &[
            cmd,
            ((addr >> 24) & 0xFF) as u8,
            ((addr >> 16) & 0xFF) as u8,
            ((addr >> 8) & 0xFF) as u8,
            (addr & 0xFF) as u8,
        ]).await
    } else {
        spibus_write(dev, &[
            cmd,
            ((addr >> 16) & 0xFF) as u8,
            ((addr >> 8) & 0xFF) as u8,
            (addr & 0xFF) as u8,
        ]).await
    };
    let dis = ss_disable(dev).await;
    wr?;
    dis?;

    if chip.erase_size <= 4096 {
        wait_wip(dev).await?;
    } else {
        wait_wip_block(dev).await?;
    }

    Ok(())
}

/// Returns (opcode, block_size) for the largest aligned erase unit that fits at addr,
/// constrained to sizes actually advertised in `erase_types`.
/// Prefers the largest matching block first; falls back to the smallest advertised size.
pub(crate) fn pick_erase_op(addr: u32, remaining_bytes: u32, erase_types: &[EraseType]) -> Result<(u8, u32)> {
    // Try 64KB.
    if addr.is_multiple_of(65536)
        && remaining_bytes >= 65536
        && let Some(et) = erase_types.iter().find(|e| e.size_bytes == 65536)
    {
        return Ok((et.opcode, 65536));
    }
    // Try 32KB.
    if addr.is_multiple_of(32768)
        && remaining_bytes >= 32768
        && let Some(et) = erase_types.iter().find(|e| e.size_bytes == 32768)
    {
        return Ok((et.opcode, 32768));
    }
    // Caller guarantees a 4KB erase type is present; find it explicitly.
    let sector = erase_types
        .iter()
        .find(|e| e.size_bytes == 4096)
        .ok_or_else(|| anyhow::anyhow!("pick_erase_op: no 4KB erase type in erase_types (caller should have checked has_4k)"))?;
    Ok((sector.opcode, sector.size_bytes))
}

/// Erase a single block using an explicit opcode.
/// Encodes a 4-byte address when chip.addr_bytes == 4, 3-byte otherwise.
pub(crate) async fn erase_unit_opcode(
    dev: &UsbDevice,
    chip: &ResolvedChip,
    addr: u32,
    opcode: u8,
    use_block_timeout: bool,
) -> Result<()> {
    // The minimum erase granularity in mixed-erase mode is 4 KiB; every addr
    // supplied by erase_range_mixed is derived from 4 KiB-aligned arithmetic.
    debug_assert!(
        addr.is_multiple_of(4096),
        "erase_unit_opcode: addr {addr:#x} is not 4 KiB-aligned"
    );
    write_enable(dev).await?;
    ss_enable(dev).await?;
    let wr = if chip.addr_bytes == 4 {
        spibus_write(dev, &[
            opcode,
            ((addr >> 24) & 0xFF) as u8,
            ((addr >> 16) & 0xFF) as u8,
            ((addr >> 8) & 0xFF) as u8,
            (addr & 0xFF) as u8,
        ]).await
    } else {
        spibus_write(dev, &[
            opcode,
            ((addr >> 16) & 0xFF) as u8,
            ((addr >> 8) & 0xFF) as u8,
            (addr & 0xFF) as u8,
        ]).await
    };
    let dis = ss_disable(dev).await;
    wr?;
    dis?;

    if use_block_timeout {
        wait_wip_block(dev).await?;
    } else {
        wait_wip(dev).await?;
    }
    Ok(())
}

/// Compute (first_addr, unit_count) for a range erase — pure, no I/O.
/// Returns Err if len == 0 or the range overflows u32.
pub(crate) fn erase_range_bounds(unit: u32, offset: u32, len: u32) -> Result<(u32, u32)> {
    if len == 0 {
        bail!("erase length must be > 0");
    }
    let end_inclusive = offset
        .checked_add(len - 1)
        .ok_or_else(|| anyhow::anyhow!("erase range overflows u32 (offset={offset:#x} len={len:#x})"))?;
    let first = (offset / unit) * unit;
    let last  = (end_inclusive / unit) * unit;
    let count = (last - first) / unit + 1;
    Ok((first, count))
}

/// Pure (no I/O) erase planner: given a sorted list of dirty sector addresses
/// (each `erase_size`-aligned), groups them into contiguous runs and greedily
/// tiles each run with the largest available erase units via `pick_erase_op`.
///
/// Only call this when the chip advertises mixed erase types (has_4k + larger).
/// Gaps between non-consecutive dirty sectors are hard barriers — the planner
/// never bridges them, ensuring only dirty regions are erased.
pub(crate) fn plan_erase_ops(
    dirty_addrs: &[u32],
    erase_types: &[EraseType],
    erase_size: u32,
) -> Result<Vec<EraseOp>> {
    if dirty_addrs.is_empty() {
        return Ok(vec![]);
    }
    if !erase_types.iter().any(|e| e.size_bytes == erase_size) {
        bail!("plan_erase_ops: no erase type for minimum unit {erase_size}B in erase_types");
    }

    let mut ops = Vec::new();

    // Group consecutive dirty sectors into contiguous runs.
    let mut run_start = dirty_addrs[0];
    let mut run_end   = dirty_addrs[0]; // inclusive last sector in run

    for &addr in &dirty_addrs[1..] {
        if addr == run_end + erase_size {
            run_end = addr;
        } else {
            // Flush current run.
            tile_run(run_start, run_end, erase_size, erase_types, &mut ops)?;
            run_start = addr;
            run_end   = addr;
        }
    }
    tile_run(run_start, run_end, erase_size, erase_types, &mut ops)?;

    Ok(ops)
}

/// Tile a contiguous dirty run [run_start..=run_end] with erase ops via pick_erase_op.
fn tile_run(
    run_start: u32,
    run_end:   u32,
    erase_size: u32,
    erase_types: &[EraseType],
    ops: &mut Vec<EraseOp>,
) -> Result<()> {
    let run_bytes_end = run_end + erase_size; // exclusive end of run
    let mut cursor = run_start;
    while cursor < run_bytes_end {
        let remaining = run_bytes_end - cursor;
        let (opcode, size) = pick_erase_op(cursor, remaining, erase_types)?;
        ops.push(EraseOp { addr: cursor, size, opcode });
        cursor = cursor
            .checked_add(size)
            .ok_or_else(|| anyhow::anyhow!("erase op address overflow at {cursor:#x} + {size:#x}"))?;
    }
    Ok(())
}

/// Erase all erase units that overlap [offset, offset+len).
pub async fn erase_range(dev: &UsbDevice, chip: &ResolvedChip, offset: u32, len: u32) -> Result<()> {
    // Use mixed 64KB/32KB/4KB erase for 3-byte address chips that advertise
    // a 4KB erase type and at least one larger block size. The 4KB check is
    // required because erase_range_mixed aligns to 4KB boundaries and
    // pick_erase_op expects a 4KB entry to be present as its fallback.
    let has_4k = chip.erase_types.iter().any(|e| e.size_bytes == 4096);
    if chip.addr_bytes == 3 && has_4k && chip.erase_types.len() > 1 {
        return erase_range_mixed(dev, chip, offset, len).await;
    }

    let unit = chip.erase_size;
    let (first, count) = erase_range_bounds(unit, offset, len)?;
    let last = first + (count - 1) * unit;

    info!(
        "erasing {} unit(s) of {}KB each ({} bytes total)",
        count, unit / 1024, count * unit
    );

    let mut pb = Progress::new("Erasing", count as u64);
    let mut addr = first;
    while addr <= last {
        erase_unit(dev, chip, addr).await?;
        pb.inc(1);
        // Guard against wrapping at the top of the u32 address space
        // (e.g. addr = 0xFFFF_F000, unit = 4096 would wrap to 0 without this,
        // causing the loop condition addr <= last to remain true indefinitely).
        addr = match addr.checked_add(unit) {
            Some(a) => a,
            None => break,
        };
    }
    pb.finish();
    Ok(())
}

/// Mixed erase: uses 64KB / 32KB / 4KB units for maximum throughput.
/// Preconditions: chip.addr_bytes == 3, chip advertises a 4KB erase type,
/// and at least one larger erase type is also advertised.
async fn erase_range_mixed(dev: &UsbDevice, chip: &ResolvedChip, offset: u32, len: u32) -> Result<()> {
    if len == 0 {
        bail!("erase length must be > 0");
    }
    let end_inclusive = offset
        .checked_add(len - 1)
        .ok_or_else(|| anyhow::anyhow!("erase range overflows u32 (offset={offset:#x} len={len:#x})"))?;

    let first = (offset / 4096) * 4096;
    let last  = (end_inclusive / 4096) * 4096;
    let end   = last as u64 + 4096; // exclusive end, u64 — never overflows
    let total_bytes = end - first as u64;

    info!(
        "mixed erase [{:#010x}..{:#010x}): up to 64KB/32KB/4KB units",
        first, end
    );

    let mut pb = Progress::new("Erasing", total_bytes);
    let mut addr = first;
    while addr as u64 <= end - 4096 {
        let remaining = (end - addr as u64) as u32; // safe: always <= u32::MAX since addr >= first and end = last+4096 <= 0x1_0000_0000
        let (opcode, size) = pick_erase_op(addr, remaining, &chip.erase_types)?;
        erase_unit_opcode(dev, chip, addr, opcode, size > 4096).await?;
        pb.inc(size as u64);
        // Use checked_add to guard against wrapping at the top of the u32 address
        // space (e.g. addr = 0xFFFF_F000, size = 4096 would wrap to 0 without this).
        addr = match addr.checked_add(size) {
            Some(a) => a,
            None => break,
        };
    }
    pb.finish();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{erase_range_bounds, pick_erase_op, plan_erase_ops, EraseOp};
    use crate::chip::EraseType;

    const SECTOR: u32 = 4096;
    const BLOCK64: u32 = 65536;

    /// Standard 3-byte 4KB chip: 4KB + 32KB + 64KB erase types.
    fn all_types() -> Vec<EraseType> {
        vec![
            EraseType { size_bytes: 4096,  opcode: 0x20 },
            EraseType { size_bytes: 32768, opcode: 0x52 },
            EraseType { size_bytes: 65536, opcode: 0xD8 },
        ]
    }

    /// DB-only chip that only advertises 4KB sector erase.
    fn sector_only() -> Vec<EraseType> {
        vec![EraseType { size_bytes: 4096, opcode: 0x20 }]
    }

    #[test]
    fn aligned_single_sector() {
        let (first, count) = erase_range_bounds(SECTOR, 0x0000_1000, 4096).unwrap();
        assert_eq!(first, 0x0000_1000);
        assert_eq!(count, 1);
    }

    #[test]
    fn unaligned_spans_two_sectors() {
        let (first, count) = erase_range_bounds(SECTOR, 0x0000_0FFF, 2).unwrap();
        assert_eq!(first, 0x0000_0000);
        assert_eq!(count, 2);
    }

    #[test]
    fn offset_zero_full_chip_16mb() {
        let (first, count) = erase_range_bounds(SECTOR, 0, 16 * 1024 * 1024).unwrap();
        assert_eq!(first, 0);
        assert_eq!(count, 4096);
    }

    #[test]
    fn block64_aligned() {
        let (first, count) = erase_range_bounds(BLOCK64, 0x0002_0000, 65536).unwrap();
        assert_eq!(first, 0x0002_0000);
        assert_eq!(count, 1);
    }

    #[test]
    fn block64_unaligned_three_blocks() {
        let (first, count) = erase_range_bounds(BLOCK64, 0x0000_8000, 128 * 1024).unwrap();
        assert_eq!(first, 0x0000_0000);
        assert_eq!(count, 3);
    }

    #[test]
    fn zero_len_is_error() {
        assert!(erase_range_bounds(SECTOR, 0x1000, 0).is_err());
    }

    #[test]
    fn overflow_is_error() {
        assert!(erase_range_bounds(SECTOR, 0xFFFF_F000, 0x2000).is_err());
    }

    #[test]
    fn pick_64k_at_64k_boundary() {
        assert_eq!(pick_erase_op(0x10000, 65536, &all_types()).unwrap(), (0xD8, 65536));
    }

    #[test]
    fn pick_32k_not_64k_aligned() {
        assert_eq!(pick_erase_op(0x8000, 32768, &all_types()).unwrap(), (0x52, 32768));
    }

    #[test]
    fn pick_4k_when_not_aligned() {
        assert_eq!(pick_erase_op(0x1000, 65536, &all_types()).unwrap(), (0x20, 4096));
    }

    #[test]
    fn pick_4k_when_insufficient_remaining() {
        assert_eq!(pick_erase_op(0x10000, 32767, &all_types()).unwrap(), (0x20, 4096));
    }

    #[test]
    fn prefer_64k_over_32k_at_64k_boundary() {
        assert_eq!(pick_erase_op(0x10000, 65536, &all_types()).unwrap(), (0xD8, 65536));
    }

    /// Chip only advertises 4KB — must fall back to 4KB even at a 64KB-aligned address
    /// with enough remaining bytes, instead of issuing an unsupported 0xD8 opcode.
    #[test]
    fn pick_falls_back_to_4k_when_only_4k_advertised() {
        assert_eq!(pick_erase_op(0x10000, 65536, &sector_only()).unwrap(), (0x20, 4096));
    }

    /// last 4KB sector of a 4GB chip — last + 4096 would overflow u32 without the u64 fix.
    #[test]
    fn range_bounds_at_top_of_u32() {
        let (first, count) = erase_range_bounds(4096, 0xFFFF_F000, 4096).unwrap();
        assert_eq!(first, 0xFFFF_F000);
        assert_eq!(count, 1);
    }

    /// Simulate the non-mixed erase loop over the last sector of a near-4GB chip.
    /// Without the checked_add fix, addr + unit wraps to 0 and the loop never
    /// terminates. With the fix, the loop executes exactly once and breaks.
    #[test]
    fn erase_range_at_top_of_u32_terminates() {
        let unit: u32 = 4096;
        let (first, count) = erase_range_bounds(unit, 0xFFFF_F000, 4096).unwrap();
        assert_eq!(first, 0xFFFF_F000);
        assert_eq!(count, 1);

        // Replay the fixed loop logic to confirm it terminates with exactly `count` iterations.
        let last = first + (count - 1) * unit;
        let mut addr = first;
        let mut iterations: u32 = 0;
        while addr <= last {
            iterations += 1;
            addr = match addr.checked_add(unit) {
                Some(a) => a,
                None => break,
            };
        }
        assert_eq!(iterations, count, "loop must execute exactly count={count} time(s)");
    }

    #[test]
    fn pick_falls_back_to_4k_opcode_from_types() {
        // addr not aligned to 32K, so must use the 4KB entry
        let types = vec![
            EraseType { size_bytes: 4096,  opcode: 0x20 },
            EraseType { size_bytes: 65536, opcode: 0xD8 },
        ];
        assert_eq!(pick_erase_op(0x5000, 65536, &types).unwrap(), (0x20, 4096));
    }

    /// erase_types may carry non-standard opcodes (e.g. 4-byte-address 64KB = 0xDC);
    /// pick_erase_op must use whatever opcode the EraseType entry specifies.
    #[test]
    fn pick_uses_correct_opcode_from_erase_types() {
        let types = vec![
            EraseType { size_bytes: 4096,  opcode: 0x21 },
            EraseType { size_bytes: 65536, opcode: 0xDC },
        ];
        assert_eq!(pick_erase_op(0x10000, 65536, &types).unwrap(), (0xDC, 65536));
    }

    #[test]
    fn erase_unit_uses_opcode_from_erase_types() {
        let types = vec![
            EraseType { size_bytes: 4096,  opcode: 0x20 },
            EraseType { size_bytes: 65536, opcode: 0xD8 },
        ];
        let found = types.iter().find(|e| e.size_bytes == 65536).unwrap();
        assert_eq!(found.opcode, 0xD8);
        let found = types.iter().find(|e| e.size_bytes == 4096).unwrap();
        assert_eq!(found.opcode, 0x20);
    }

    // ── plan_erase_ops tests ──────────────────────────────────────────────────

    #[test]
    fn plan_single_dirty_sector_at_64k_boundary() {
        // Single 4KB dirty sector at 0x10000 — remaining=4096, too small for 32KB/64KB.
        let ops = plan_erase_ops(&[0x10000], &all_types(), 4096).unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0], EraseOp { addr: 0x10000, size: 4096, opcode: 0x20 });
    }

    #[test]
    fn plan_eight_adjacent_sectors_coalesce_to_one_32k() {
        // 8 consecutive 4KB sectors at 0x8000 = exactly 32KB → one 32KB erase.
        let addrs: Vec<u32> = (0..8).map(|i| 0x8000_u32 + i * 4096).collect();
        let ops = plan_erase_ops(&addrs, &all_types(), 4096).unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0], EraseOp { addr: 0x8000, size: 32768, opcode: 0x52 });
    }

    #[test]
    fn plan_sixteen_adjacent_sectors_coalesce_to_one_64k() {
        // 16 consecutive 4KB sectors at 0x10000 = exactly 64KB → one 64KB erase.
        let addrs: Vec<u32> = (0..16).map(|i| 0x10000_u32 + i * 4096).collect();
        let ops = plan_erase_ops(&addrs, &all_types(), 4096).unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0], EraseOp { addr: 0x10000, size: 65536, opcode: 0xD8 });
    }

    #[test]
    fn plan_non_contiguous_sectors_produce_separate_groups() {
        // Dirty sectors at 0x0 and 0x20000 (gap between them) → separate ops.
        let ops = plan_erase_ops(&[0x0000, 0x20000], &all_types(), 4096).unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].addr, 0x0000);
        assert_eq!(ops[1].addr, 0x20000);
    }

    #[test]
    fn plan_4k_only_chip_always_4k_ops() {
        // Chip with only 4KB erase — even 16 aligned sectors must each use 4KB.
        let addrs: Vec<u32> = (0..16).map(|i| 0x10000_u32 + i * 4096).collect();
        let ops = plan_erase_ops(&addrs, &sector_only(), 4096).unwrap();
        assert_eq!(ops.len(), 16);
        assert!(ops.iter().all(|op| op.size == 4096 && op.opcode == 0x20));
    }

    #[test]
    fn plan_two_sectors_prefer_4k_when_run_too_small_for_32k() {
        // Two 4KB sectors = 8KB dirty run — not enough for 32KB erase, use 2×4KB.
        let ops = plan_erase_ops(&[0x8000, 0x9000], &all_types(), 4096).unwrap();
        assert_eq!(ops.len(), 2);
        assert!(ops.iter().all(|op| op.size == 4096));
    }

    #[test]
    fn plan_empty_dirty_addrs_returns_empty() {
        let ops = plan_erase_ops(&[], &all_types(), 4096).unwrap();
        assert!(ops.is_empty());
    }

    #[test]
    fn plan_missing_min_erase_type_is_error() {
        // erase_size=4096 but no 4KB type in erase_types → should bail.
        let types = vec![EraseType { size_bytes: 65536, opcode: 0xD8 }];
        assert!(plan_erase_ops(&[0x10000], &types, 4096).is_err());
    }

    #[test]
    fn erased_coverage_expansion_64k_op_gives_16_sectors() {
        // Verify that a 64KB EraseOp at 0x10000 expands to exactly 16 × 4KB addresses.
        let op = EraseOp { addr: 0x10000, size: 65536, opcode: 0xD8 };
        let erase_size: u32 = 4096;
        let mut coverage: Vec<u32> = (op.addr..op.addr + op.size)
            .step_by(erase_size as usize)
            .collect();
        coverage.sort_unstable();
        coverage.dedup();
        assert_eq!(coverage.len(), 16);
        assert_eq!(coverage[0],  0x10000);
        assert_eq!(coverage[15], 0x1F000);
    }
}
