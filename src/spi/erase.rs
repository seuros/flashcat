use anyhow::{bail, Result};
use tracing::info;

use crate::chip::ResolvedChip;
use crate::progress::Progress;
use crate::usb::UsbDevice;

use super::bus::{spibus_write, ss_disable, ss_enable};
use super::write::{wait_wip, wait_wip_block, wait_wip_long, write_enable};

pub async fn erase_chip(dev: &UsbDevice, chip: &ResolvedChip) -> Result<()> {
    info!("chip erase: {} ({} bytes)", chip.name, chip.size_bytes);
    write_enable(dev).await?;
    ss_enable(dev).await?;
    spibus_write(dev, &[0xC7]).await?; // CE
    ss_disable(dev).await?;
    wait_wip_long(dev).await
}

/// Erase a single aligned sector/block at `addr`.
/// 3-byte chips: 0x20 (4KB SE) / 0xD8 (64KB BE).
/// 4-byte chips: 0x21 (4KB SE4B) / 0xDC (64KB BE4B) — avoids EN4B mode entry.
pub async fn erase_unit(dev: &UsbDevice, chip: &ResolvedChip, addr: u32) -> Result<()> {
    let cmd: u8 = match (chip.addr_bytes, chip.erase_size <= 4096) {
        (4, true)  => 0x21, // Sector Erase 4-byte address
        (4, false) => 0xDC, // Block Erase 64KB 4-byte address
        (_, true)  => 0x20, // Sector Erase 3-byte address
        (_, false) => 0xD8, // Block Erase 64KB 3-byte address
    };

    write_enable(dev).await?;
    ss_enable(dev).await?;
    if chip.addr_bytes == 4 {
        spibus_write(dev, &[
            cmd,
            ((addr >> 24) & 0xFF) as u8,
            ((addr >> 16) & 0xFF) as u8,
            ((addr >> 8) & 0xFF) as u8,
            (addr & 0xFF) as u8,
        ]).await?;
    } else {
        spibus_write(dev, &[
            cmd,
            ((addr >> 16) & 0xFF) as u8,
            ((addr >> 8) & 0xFF) as u8,
            (addr & 0xFF) as u8,
        ]).await?;
    }
    ss_disable(dev).await?;

    if chip.erase_size <= 4096 {
        wait_wip(dev).await?;
    } else {
        wait_wip_block(dev).await?;
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

/// Erase all erase units that overlap [offset, offset+len).
pub async fn erase_range(dev: &UsbDevice, chip: &ResolvedChip, offset: u32, len: u32) -> Result<()> {
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
        addr += unit;
    }
    pb.finish();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::erase_range_bounds;

    const SECTOR: u32 = 4096;
    const BLOCK64: u32 = 65536;

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
}
