use anyhow::{bail, Result};
use std::time::Duration;

use crate::chip::ResolvedChip;
use crate::progress::Progress;
use crate::usb::{UsbDevice, UsbReq};
use tracing::{debug, info};

use super::bus::{spibus_read, spibus_write, ss_disable, ss_enable};
use super::erase::erase_unit;
use super::read::read;

// Firmware handles multi-page writes internally (WREN+PAGE_PROGRAM+WIP per page).
// Send up to 64KB per ctrl_out/bulk_out cycle to minimize USB round trips.
const WRITE_BLOCK: u32 = 65536;

fn rotate_pages_left(data: &[u8], page_size: usize) -> Vec<u8> {
    if page_size == 0 {
        return data.to_vec();
    }

    let mut rotated = Vec::with_capacity(data.len());
    for page in data.chunks(page_size) {
        if page.len() <= 1 {
            rotated.extend_from_slice(page);
            continue;
        }
        rotated.extend_from_slice(&page[1..]);
        rotated.push(page[0]);
    }
    rotated
}

pub async fn write(dev: &UsbDevice, chip: &ResolvedChip, offset: u32, data: &[u8]) -> Result<()> {
    let total = data.len() as u64;
    let mut pb = Progress::new("Writing", total);
    let mut addr = offset;
    let mut remaining = data;

    while !remaining.is_empty() {
        // First chunk: align to page boundary if mid-page
        let page_offset = addr % chip.page_size;
        let chunk_size = if page_offset > 0 {
            (chip.page_size - page_offset).min(remaining.len() as u32) as usize
        } else {
            WRITE_BLOCK.min(remaining.len() as u32) as usize
        };
        let (chunk, rest) = remaining.split_at(chunk_size);

        write_block(dev, chip, addr, chunk).await?;

        addr += chunk_size as u32;
        remaining = rest;
        pb.inc(chunk_size as u64);
    }

    pb.finish();
    Ok(())
}

/// Read-compare-erase-write: only touches sectors that differ, skips all-0xFF pages.
/// Equivalent to flashrom's default write strategy.
pub async fn write_smart(dev: &UsbDevice, chip: &ResolvedChip, offset: u32, data: &[u8]) -> Result<()> {
    let erase_size = chip.erase_size as usize;
    let page_size  = chip.page_size as usize;

    // Read current flash content in the target range
    let current = read(dev, chip, offset, data.len() as u32, false).await?;

    // Sector-aligned iteration over the write range
    let first_sector = (offset / chip.erase_size) * chip.erase_size;
    let last_sector  = ((offset + data.len() as u32).div_ceil(chip.erase_size)) * chip.erase_size;

    let mut sectors_skipped = 0u32;
    let mut sectors_erased  = 0u32;
    let mut pages_skipped   = 0u32;
    let mut pages_written   = 0u32;

    let mut pb = Progress::new("Writing", data.len() as u64);

    let mut sector_base = first_sector;
    while sector_base < last_sector {
        // Clamp to the data range (first/last sectors may be partial)
        let data_start = (sector_base as usize).saturating_sub(offset as usize);
        let data_end   = ((sector_base as usize + erase_size).saturating_sub(offset as usize))
            .min(data.len());

        let target  = &data[data_start..data_end];
        let current_sector = &current[data_start..data_end];

        if target == current_sector {
            sectors_skipped += 1;
            pb.inc((data_end - data_start) as u64);
            sector_base += chip.erase_size;
            continue;
        }

        // Erase if any bit needs to go 0→1
        let needs_erase = target.iter().zip(current_sector).any(|(t, e)| t & !e != 0);
        if needs_erase {
            erase_unit(dev, chip, sector_base).await?;
            sectors_erased += 1;
        }

        let mut page_off = 0usize;
        while page_off < target.len() {
            let page_end = (page_off + page_size).min(target.len());
            let page = &target[page_off..page_end];

            // Skip pages that are all 0xFF (already erased / no-op)
            if page.iter().all(|&b| b == 0xFF) {
                pages_skipped += 1;
                pb.inc((page_end - page_off) as u64);
                page_off = page_end;
                continue;
            }

            // Batch consecutive non-0xFF pages into one write_block (up to WRITE_BLOCK).
            let mut run_end = page_end;
            while run_end < target.len()
                && run_end - page_off < WRITE_BLOCK as usize
                && !target[run_end..((run_end + page_size).min(target.len()))].iter().all(|&b| b == 0xFF)
            {
                run_end = (run_end + page_size).min(target.len());
            }

            let write_addr = sector_base + page_off as u32;
            write_block(dev, chip, write_addr, &target[page_off..run_end]).await?;
            pages_written += ((run_end - page_off).div_ceil(page_size)) as u32;
            pb.inc((run_end - page_off) as u64);
            page_off = run_end;
        }
        sector_base += chip.erase_size;
    }

    pb.finish();
    info!(
        "smart write: {} sectors skipped, {} erased, {} pages written, {} pages skipped (0xFF)",
        sectors_skipped, sectors_erased, pages_written, pages_skipped
    );
    Ok(())
}

async fn write_block(dev: &UsbDevice, chip: &ResolvedChip, addr: u32, data: &[u8]) -> Result<()> {
    // ctrl_out arms the firmware, bulk_out must follow without delay
    let setup = write_setup_packet(chip, addr, data.len() as u32);
    let rotated = rotate_pages_left(data, chip.page_size as usize);
    dev.ctrl_out_nodelay(UsbReq::SpiWriteFlash, 0, Some(&setup)).await?;
    dev.bulk_out(rotated).await?;
    // Wait for the last page in the block to finish programming.
    // Firmware polls WIP internally between pages; we only wait once per block.
    let pages = data.len().div_ceil(chip.page_size as usize) as u32;
    wait_wip_write(dev, pages).await
}

pub(crate) fn write_setup_packet(chip: &ResolvedChip, offset: u32, count: u32) -> [u8; 15] {
    [
        0x02, // PAGE_PROGRAM
        0x06, // WREN
        0x05, // RDSR
        0x00, // RDFR (not used)
        chip.addr_bytes,
        ((chip.page_size >> 8) & 0xFF) as u8,
        (chip.page_size & 0xFF) as u8,
        ((offset >> 24) & 0xFF) as u8,
        ((offset >> 16) & 0xFF) as u8,
        ((offset >> 8) & 0xFF) as u8,
        (offset & 0xFF) as u8,
        ((count >> 16) & 0xFF) as u8,
        ((count >> 8) & 0xFF) as u8,
        (count & 0xFF) as u8,
        0, // SPI_ONLY
    ]
}

pub(crate) async fn write_enable(dev: &UsbDevice) -> Result<()> {
    ss_enable(dev).await?;
    spibus_write(dev, &[0x06]).await?; // WREN
    ss_disable(dev).await
}

async fn poll_wip(dev: &UsbDevice, max_polls: u32, interval_ms: u64) -> Result<()> {
    for _ in 0..max_polls {
        tokio::time::sleep(Duration::from_millis(interval_ms)).await;
        ss_enable(dev).await?;
        spibus_write(dev, &[0x05]).await?; // RDSR
        let sr = spibus_read(dev, 1).await?;
        ss_disable(dev).await?;
        if sr.first().map(|b| b & 0x01).unwrap_or(1) == 0 {
            return Ok(());
        }
    }
    bail!("WIP timeout after {}ms", max_polls as u64 * interval_ms);
}

pub(crate) async fn wait_wip(dev: &UsbDevice) -> Result<()> {
    poll_wip(dev, 1000, 2).await // 2s — single page program
}

/// Wait for WIP after a multi-page write block. 3ms max per page, 10ms poll interval.
pub(crate) async fn wait_wip_write(dev: &UsbDevice, pages: u32) -> Result<()> {
    let timeout_ms = (pages as u64 * 3).max(100);
    let interval_ms = 2u64;
    let max_polls = timeout_ms.div_ceil(interval_ms) as u32;
    poll_wip(dev, max_polls, interval_ms).await
}

pub(crate) async fn wait_wip_block(dev: &UsbDevice) -> Result<()> {
    poll_wip(dev, 500, 10).await // 5s — 64KB block erase
}

pub(crate) async fn wait_wip_chip_erase(dev: &UsbDevice, chip: &ResolvedChip) -> Result<()> {
    let timeout_ms = chip.chip_erase_timeout_ms();
    let interval_ms = 10u64;
    let max_polls = timeout_ms.div_ceil(interval_ms) as u32;
    debug!("chip erase poll: {}ms timeout ({} polls)", timeout_ms, max_polls);
    poll_wip(dev, max_polls, interval_ms).await
}

#[cfg(test)]
mod tests {
    use super::rotate_pages_left;

    #[test]
    fn rotate_single_full_page_left_by_one() {
        let data = vec![0x00, 0x01, 0x02, 0x03];
        let rotated = rotate_pages_left(&data, 4);
        assert_eq!(rotated, vec![0x01, 0x02, 0x03, 0x00]);
    }

    #[test]
    fn rotate_two_pages_independently() {
        let data = vec![0x00, 0x01, 0x02, 0x03, 0x10, 0x11, 0x12, 0x13];
        let rotated = rotate_pages_left(&data, 4);
        assert_eq!(rotated, vec![0x01, 0x02, 0x03, 0x00, 0x11, 0x12, 0x13, 0x10]);
    }

    #[test]
    fn rotate_partial_last_page_within_its_own_length() {
        let data = vec![0x00, 0x01, 0x02, 0x03, 0xA0, 0xA1, 0xA2];
        let rotated = rotate_pages_left(&data, 4);
        assert_eq!(rotated, vec![0x01, 0x02, 0x03, 0x00, 0xA1, 0xA2, 0xA0]);
    }

    #[test]
    fn rotate_all_ff_page_stays_ff() {
        let data = vec![0xFF; 256];
        let rotated = rotate_pages_left(&data, 256);
        assert_eq!(rotated, data);
    }
}
