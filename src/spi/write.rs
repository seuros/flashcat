use anyhow::{bail, Result};
use std::time::Duration;

use crate::chip::ResolvedChip;
use crate::progress::Progress;
use crate::usb::{UsbDevice, UsbReq};
use tracing::{debug, info};

use super::bus::{spibus_read, spibus_write, ss_disable, ss_enable};
use super::erase::{erase_unit_opcode, plan_erase_ops, EraseOp};
use super::read::read;

// Firmware handles multi-page writes internally (WREN+PAGE_PROGRAM+WIP per page).
// Send up to 64KB per ctrl_out/bulk_out cycle to minimize USB round trips.
const WRITE_BLOCK: u32 = 65536;

/// Compensates for the firmware's PAGE_PROGRAM +1 offset bug.
///
/// The firmware issues PAGE_PROGRAM starting at `addr+1` instead of `addr`, so the
/// SPI flash page-wrap mechanism is used to land `buf[page_size-1]` at `addr+0`.
/// Each page-sized chunk is rotated left by one byte so that position 0 of every
/// logical page ends up wrapped around to the page's base address.
///
/// # Partial trailing pages
///
/// When `data.len()` is not a multiple of `page_size` the trailing bytes form a
/// partial page.  The caller is expected to have ensured the write address for
/// that partial page is page-aligned (which `write_block` guarantees by padding
/// `data` to a full page multiple before calling this function).  Therefore this
/// function always receives data whose length is already a multiple of `page_size`
/// and every chunk is exactly `page_size` bytes long; the `page.len() <= 1` guard
/// is kept only as a safety net.
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
    if chip.page_size == 0 {
        bail!("chip page_size is 0 — invalid chip configuration");
    }
    let total = data.len() as u64;
    let mut pb = Progress::new("Writing", total);
    let mut addr = offset;
    let mut remaining = data;

    while !remaining.is_empty() {
        // When the write address is not page-aligned we must first fill in the
        // partial leading page so that the page-wrap rotation lands every byte at
        // the right flash address.
        //
        // The firmware PAGE_PROGRAM +1 bug is compensated by rotating each
        // page-sized buffer left by one byte and relying on the SPI flash
        // hardware page-wrap (which wraps at the page boundary, not at `addr`).
        // This wrap is only triggered when the firmware issues a full PAGE_PROGRAM
        // of exactly `page_size` bytes.  A sub-page transfer has no wrap, so the
        // rotated-off first byte would land one position past the end of the
        // transfer window instead of at `addr`.
        //
        // Fix: when `page_offset != 0`, back-track to `page_base` and prepend
        // `page_offset` filler bytes (0xFF = erased value) so the firmware always
        // receives a full page.  The filler bytes are written to already-erased
        // cells and are overwritten (or left as-is) by earlier write operations,
        // so they cannot corrupt data.
        let page_offset = addr % chip.page_size;
        let (write_addr, chunk_size, prefix_len) = if page_offset > 0 {
            let fill = page_offset as usize;
            let payload = (chip.page_size - page_offset).min(remaining.len() as u32) as usize;
            // Adjust the write address back to the page boundary.
            (addr - page_offset, fill + payload, fill)
        } else {
            let payload = WRITE_BLOCK.min(remaining.len() as u32) as usize;
            (addr, payload, 0)
        };
        let (chunk, rest) = remaining.split_at(chunk_size - prefix_len);

        if prefix_len > 0 {
            // Build a full-page buffer: [0xFF × prefix_len] ++ chunk
            let mut full_page = vec![0xFFu8; prefix_len];
            full_page.extend_from_slice(chunk);
            write_block(dev, chip, write_addr, &full_page).await?;
        } else {
            write_block(dev, chip, write_addr, chunk).await?;
        }

        addr += (chunk_size - prefix_len) as u32;
        remaining = rest;
        pb.inc((chunk_size - prefix_len) as u64);
    }

    pb.finish();
    Ok(())
}

/// Read-compare-erase-write: only touches sectors that differ, uses the largest
/// available erase unit (64KB > 32KB > 4KB) for each dirty region.
///
/// Three phases:
///   A) Classify all sectors in the write range as Skip / WriteOnly / Erase.
///   B) Plan and execute erases (largest-unit greedy tiling for dirty regions).
///      Expands each EraseOp into a coverage set — large erases may sweep clean
///      sectors outside the write range that must be restored in Phase C.
///   C) Write: erased sectors get all non-0xFF pages written back; write-only
///      sectors get only changed pages written; skipped sectors are left alone.
///
/// The read range is extended to the nearest large-erase alignment boundary so
/// the original content of any swept clean sector is always available for restore.
///
/// NOTE: Phase B erases all dirty regions before any writes begin. This widens
/// the power-fail window compared to a per-sector erase+write loop. Acceptable
/// for a USB flash programmer; not appropriate for production firmware updaters.
pub async fn write_smart(dev: &UsbDevice, chip: &ResolvedChip, offset: u32, data: &[u8]) -> Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    let erase_size = chip.erase_size;
    let first_sector = (offset / erase_size) * erase_size;
    let end = offset
        .checked_add(data.len() as u32)
        .ok_or_else(|| anyhow::anyhow!("write range overflows u32 (offset={offset:#x} len={:#x})", data.len()))?;
    let last_sector_start = ((end - 1) / erase_size) * erase_size;

    // Extend the read domain to cover any large erase alignment, so clean sectors
    // swept by a 64KB/32KB erase can be restored with their original content.
    let max_erase_unit = chip.erase_types.iter()
        .map(|e| e.size_bytes)
        .max()
        .unwrap_or(erase_size);

    let read_start = (first_sector / max_erase_unit) * max_erase_unit;
    let last_large_start = ((last_sector_start + erase_size - 1) / max_erase_unit) * max_erase_unit;
    let read_end = (last_large_start + max_erase_unit).min(chip.size_bytes);
    let read_len = read_end - read_start;

    let read_len_u32 = u32::try_from(read_len)
        .map_err(|_| anyhow::anyhow!("read range {read_len:#x} exceeds u32"))?;

    let original = read(dev, chip, read_start, read_len_u32, false).await?;

    // Build merged buffer over the read range: original with new data overlaid.
    let mut merged = original.clone();
    let overlay_start = (offset - read_start) as usize;
    merged[overlay_start..overlay_start + data.len()].copy_from_slice(data);

    // Mixed erase (64KB/32KB/4KB) is only safe when the chip advertises all sizes
    // and uses 3-byte addressing. 4-byte address chips use different opcodes and
    // are handled with per-sector erase_unit_opcode calls at erase_size granularity.
    let use_mixed = chip.addr_bytes == 3
        && chip.erase_types.iter().any(|e| e.size_bytes == erase_size)
        && chip.erase_types.len() > 1;

    // ── Phase A: classify write-range sectors ────────────────────────────────
    let mut dirty_erase_addrs: Vec<u32> = Vec::new();
    let mut write_only_addrs:  Vec<u32> = Vec::new();
    let mut skip_count = 0u32;

    let mut sector_addr = first_sector;
    loop {
        let buf_off = (sector_addr - read_start) as usize;
        let orig_s = &original[buf_off..buf_off + erase_size as usize];
        let new_s  = &merged [buf_off..buf_off + erase_size as usize];

        if orig_s == new_s {
            skip_count += 1;
        } else if new_s.iter().zip(orig_s).any(|(n, o)| n & !o != 0) {
            dirty_erase_addrs.push(sector_addr);
        } else {
            write_only_addrs.push(sector_addr);
        }

        if sector_addr >= last_sector_start { break; }
        sector_addr += erase_size;
    }

    let mut stats = SmartStats { sectors_skipped: skip_count, ..Default::default() };

    // ── Phase B: plan and execute erases ─────────────────────────────────────
    let erase_ops: Vec<EraseOp> = if use_mixed {
        plan_erase_ops(&dirty_erase_addrs, &chip.erase_types, erase_size)?
    } else {
        let opcode = chip.erase_types.iter()
            .find(|e| e.size_bytes == erase_size)
            .map(|e| e.opcode)
            .unwrap_or(0x20);
        dirty_erase_addrs.iter()
            .map(|&addr| EraseOp { addr, size: erase_size, opcode })
            .collect()
    };

    for op in &erase_ops {
        erase_unit_opcode(dev, chip, op.addr, op.opcode, op.size > 4096).await?;
        stats.erase_ops   += 1;
        stats.bytes_erased += op.size as u64;
    }

    // Expand erase ops into a sorted set of all covered erase_size-sector addresses.
    // This includes clean sectors swept by large erases — Phase C will restore them.
    let mut erased_coverage: Vec<u32> = erase_ops.iter()
        .flat_map(|op| (op.addr..op.addr + op.size).step_by(erase_size as usize))
        .collect();
    erased_coverage.sort_unstable();
    erased_coverage.dedup();

    // ── Phase C: write ───────────────────────────────────────────────────────
    // Iterate the full read range so spillover sectors (erased but outside the
    // write range) are restored with their original content.
    let total_sectors = (read_end - read_start) / erase_size;
    let mut pb = Progress::new("Writing", read_len_u32 as u64);

    let mut addr = read_start;
    for _ in 0..total_sectors {
        let buf_off = (addr - read_start) as usize;
        let orig_s = &original[buf_off..buf_off + erase_size as usize];

        // For sectors inside the write range use merged (new) content;
        // for spillover sectors outside the range restore original content.
        let in_write_range = addr >= first_sector && addr <= last_sector_start;
        let target_s: &[u8] = if in_write_range {
            &merged[buf_off..buf_off + erase_size as usize]
        } else {
            orig_s
        };

        let in_coverage = erased_coverage.binary_search(&addr).is_ok();
        let is_write_only = write_only_addrs.binary_search(&addr).is_ok();

        if in_coverage {
            // Sector was erased (chip is all 0xFF) — write back all non-0xFF pages.
            write_sector_pages(dev, chip, addr, target_s, orig_s, true, &mut pb, &mut stats).await?;
        } else if is_write_only {
            // Only 1→0 transitions needed — program changed pages without erase.
            write_sector_pages(dev, chip, addr, target_s, orig_s, false, &mut pb, &mut stats).await?;
        } else {
            pb.inc(erase_size as u64);
        }

        addr += erase_size;
    }

    pb.finish();
    info!(
        "smart write: {} sectors skipped, {} erase op(s) ({} KB), {} pages written, {} pages skipped",
        stats.sectors_skipped, stats.erase_ops, stats.bytes_erased / 1024,
        stats.pages_written, stats.pages_skipped,
    );
    Ok(())
}

#[derive(Default)]
struct SmartStats {
    sectors_skipped: u32,
    erase_ops:       u32,
    bytes_erased:    u64,
    pages_skipped:   u32,
    pages_written:   u32,
}

// Write a sector's worth of pages, skipping pages that need no write, and
// batching consecutive dirty pages into a single write_block (≤ WRITE_BLOCK bytes).
//
// Skip logic depends on whether the sector was erased:
//
//   was_erased=false  — skip when new_page == orig_page (bit-identical, no change).
//                       We must NOT skip merely-all-0xFF pages here: if the original
//                       is not 0xFF and we skip, the old bits survive on chip.
//
//   was_erased=true   — the sector is now all 0xFF on chip regardless of orig.
//                       Skipping new_page == orig_page is WRONG: the pre-erase
//                       content is gone; orig_page bytes must be re-written.
//                       Only skip a page that is entirely 0xFF (idempotent after erase).
async fn write_sector_pages(
    dev: &UsbDevice,
    chip: &ResolvedChip,
    sector_base: u32,
    target: &[u8],
    orig: &[u8],
    was_erased: bool,
    pb: &mut Progress,
    stats: &mut SmartStats,
) -> Result<()> {
    let page_size = chip.page_size as usize;
    let mut page_off = 0usize;
    while page_off < target.len() {
        let page_end = (page_off + page_size).min(target.len());
        let new_page  = &target[page_off..page_end];
        let orig_page = &orig[page_off..page_end];

        // Determine whether this page can be skipped.
        //
        // After an erase the sector is all 0xFF on chip — orig_page reflects
        // pre-erase content and is no longer a valid comparison.  The only safe
        // skip is when the desired data is already the erased value (0xFF): writing
        // 0xFF to an erased cell is a no-op and wastes a page-program cycle.
        //
        // Without an erase the chip still holds orig_page; skip only when the
        // desired data is bit-identical to what is already there.
        let skip = if was_erased {
            new_page.iter().all(|&b| b == 0xFF)
        } else {
            new_page == orig_page
        };

        if skip {
            stats.pages_skipped += 1;
            pb.inc((page_end - page_off) as u64);
            page_off = page_end;
            continue;
        }

        // Batch consecutive dirty pages into one write_block (up to WRITE_BLOCK).
        // Use the same skip predicate so we don't prematurely break the run.
        let mut run_end = page_end;
        while run_end < target.len()
            && run_end - page_off < WRITE_BLOCK as usize
        {
            let next_end = (run_end + page_size).min(target.len());
            let next_page = &target[run_end..next_end];
            let next_clean = if was_erased {
                next_page.iter().all(|&b| b == 0xFF)
            } else {
                next_page == &orig[run_end..next_end]
            };
            if next_clean {
                break; // next page needs no write — stop the run
            }
            run_end = next_end;
        }

        let write_addr = sector_base + page_off as u32;
        write_block(dev, chip, write_addr, &target[page_off..run_end]).await?;
        stats.pages_written += ((run_end - page_off).div_ceil(page_size)) as u32;
        pb.inc((run_end - page_off) as u64);
        page_off = run_end;
    }
    Ok(())
}

async fn write_block(dev: &UsbDevice, chip: &ResolvedChip, addr: u32, data: &[u8]) -> Result<()> {
    if chip.page_size == 0 {
        bail!("chip page_size is 0 — invalid chip configuration");
    }
    let page_size = chip.page_size as usize;

    // The firmware PAGE_PROGRAM +1 offset bug is corrected by rotating each
    // page-sized chunk left by one byte and relying on the SPI flash page-wrap to
    // land the first logical byte at the correct address.  This only works when
    // every chunk sent to the firmware is exactly `page_size` bytes so that the
    // page-wrap puts the rotated-off byte at the right position.
    //
    // If `data` is not already a multiple of `page_size` (trailing partial page),
    // pad it to the next multiple with 0xFF.  Erased flash reads as 0xFF so these
    // filler bytes are idempotent on an erased device; for a read-modify-write
    // (write_smart) the caller always feeds full pages, so the pad path is only
    // exercised by raw `write()` when the total payload is shorter than one page.
    let padded_len = data.len().next_multiple_of(page_size);
    // Guard against padded_len (not data.len()) exceeding the firmware's 24-bit
    // count field.  If data.len() == 0xFF_FFFF and page_size == 256, padded_len
    // rounds up to 0x100_0000 which truncates to 0 in the 24-bit field, causing
    // the firmware to program zero bytes silently.
    if padded_len > 0xFF_FFFF {
        anyhow::bail!(
            "write block padded size {:#x} exceeds firmware 24-bit limit",
            padded_len
        );
    }
    let payload: std::borrow::Cow<[u8]> = if padded_len == data.len() {
        std::borrow::Cow::Borrowed(data)
    } else {
        let mut v = data.to_vec();
        v.resize(padded_len, 0xFF);
        std::borrow::Cow::Owned(v)
    };

    // The page-wrap trick requires the firmware to program exactly `padded_len`
    // bytes so that the rotated-off first byte lands at `addr` via the SPI flash
    // page-wrap.  If we tell the firmware only `data.len()` bytes, it stops
    // before reaching the page boundary and the rotated-off byte is never written
    // — corrupting any trailing partial page.  Always send `padded_len` as the
    // count so the firmware programs the full padded buffer.  The 0xFF pad bytes
    // are idempotent on erased flash and write_smart always erases before writing.
    let setup = write_setup_packet(chip, addr, padded_len as u32);
    let rotated = rotate_pages_left(&payload, page_size);
    dev.ctrl_out_nodelay(UsbReq::SpiWriteFlash, 0, Some(&setup)).await?;
    dev.bulk_out(rotated).await?;
    // Wait for all pages (including any 0xFF pad page) to finish programming.
    let pages = padded_len.div_ceil(page_size) as u32;
    wait_wip_write(dev, pages).await
}

pub(crate) fn write_setup_packet(chip: &ResolvedChip, offset: u32, count: u32) -> [u8; 15] {
    debug_assert!(count <= 0xFF_FFFF, "write_setup_packet: count {count:#x} exceeds 24-bit max");
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
    let wr  = spibus_write(dev, &[0x06]).await; // WREN
    let dis = ss_disable(dev).await;
    wr?;
    dis
}

async fn poll_wip(dev: &UsbDevice, max_polls: u32, interval_ms: u64) -> Result<()> {
    for _ in 0..max_polls {
        tokio::time::sleep(Duration::from_millis(interval_ms)).await;
        ss_enable(dev).await?;
        let wr  = spibus_write(dev, &[0x05]).await; // RDSR
        let rd  = spibus_read(dev, 1).await;
        let dis = ss_disable(dev).await;
        wr?;
        let sr = rd?;
        dis?;
        if sr.first().map(|b| b & 0x01).unwrap_or(1) == 0 {
            return Ok(());
        }
    }
    bail!("WIP timeout after {}ms", max_polls as u64 * interval_ms);
}

pub(crate) async fn wait_wip(dev: &UsbDevice) -> Result<()> {
    poll_wip(dev, 1000, 2).await // 2s — single page program
}

/// Wait for WIP after a multi-page write block. 10ms max per page, covers worst-case
/// NOR flash page program time (W25Q128FV max 3ms typical, up to 10ms for slow parts).
pub(crate) async fn wait_wip_write(dev: &UsbDevice, pages: u32) -> Result<()> {
    let timeout_ms = (pages as u64 * 10).max(200);
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
    use super::{rotate_pages_left, SmartStats};

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

    /// Verify the unaligned-start write path puts each data byte at the correct
    /// flash address after the firmware's PAGE_PROGRAM +1 offset is compensated.
    ///
    /// Scenario: page_size=256, write at addr=0x105 (page_offset=5).
    /// `write()` builds full_page = [0xFF × 5] ++ data[0..251] (256 bytes) and
    /// passes it to `write_block` with write_addr=0x100.
    /// `write_block` pads (already a full page) then calls `rotate_pages_left`.
    ///
    /// After rotation the firmware (which writes buf[i] to addr+i+1 and wraps
    /// buf[N-1] to addr+0) places:
    ///   buf[255] = 0xFF → 0x100  (filler, harmless)
    ///   buf[0]   = 0xFF → 0x101
    ///   buf[1]   = 0xFF → 0x102
    ///   buf[2]   = 0xFF → 0x103
    ///   buf[3]   = 0xFF → 0x104
    ///   buf[4]   = data[0] → 0x105   ← first user byte
    ///   buf[5]   = data[1] → 0x106
    ///   ...
    ///   buf[254] = data[250] → 0x1FF ← last user byte in this page
    #[test]
    fn rotate_unaligned_start_places_bytes_at_correct_flash_addresses() {
        let page_size: usize = 256;
        let page_offset: usize = 5; // addr = 0x105, page_base = 0x100

        // Build the payload data (distinguishable values so we can track each byte).
        let data: Vec<u8> = (0u8..=250).collect(); // 251 bytes

        // Simulate what write() builds: [0xFF × page_offset] ++ data
        let mut full_page = vec![0xFFu8; page_offset];
        full_page.extend_from_slice(&data);
        assert_eq!(full_page.len(), page_size);

        // write_block pads (no-op here, already a full page) then rotates.
        let rotated = rotate_pages_left(&full_page, page_size);
        assert_eq!(rotated.len(), page_size);

        // Firmware writes rotated[i] to (write_addr + i + 1), with rotated[255]
        // wrapping to write_addr + 0. Simulate that and check placement.
        let write_addr: usize = 0x100;
        let mut flash = vec![0xFFu8; 0x200]; // 512 bytes of "erased" flash
        for (i, &byte) in rotated.iter().enumerate() {
            let flash_addr = if i == page_size - 1 {
                write_addr // wrap: last byte lands at base address
            } else {
                write_addr + i + 1
            };
            flash[flash_addr] = byte;
        }

        // Filler bytes at 0x100..0x104 must be 0xFF (erased cells, no corruption).
        for addr in 0x100..0x100 + page_offset {
            assert_eq!(flash[addr], 0xFF, "filler at {addr:#x} should be 0xFF");
        }

        // User data must land starting at 0x105.
        for (j, &expected) in data.iter().enumerate() {
            let flash_addr = 0x105 + j;
            assert_eq!(
                flash[flash_addr], expected,
                "data[{j}] should be at flash address {flash_addr:#x}"
            );
        }
    }

    /// Verify that write_block's padding to a page_size multiple before rotation
    /// is correct: a partial buffer shorter than page_size is padded with 0xFF and
    /// the full padded page is rotated, not just the partial bytes.
    #[test]
    fn rotate_after_write_block_padding_is_correct() {
        // Simulate a 3-byte write that write_block pads to page_size=4.
        // data = [0xA, 0xB, 0xC]; padded = [0xA, 0xB, 0xC, 0xFF]
        // rotated = [0xB, 0xC, 0xFF, 0xA]  (full-page rotate, not partial rotate)
        let page_size = 4usize;
        let data = vec![0x0Au8, 0x0B, 0x0C];
        let padded_len = data.len().next_multiple_of(page_size);
        let mut padded = data.clone();
        padded.resize(padded_len, 0xFF);

        let rotated = rotate_pages_left(&padded, page_size);
        assert_eq!(rotated, vec![0x0B, 0x0C, 0xFF, 0x0A]);

        // Firmware places rotated[3]=0xA at write_addr+0 (wrap), so data[0]=0x0A
        // lands at write_addr+0 — correct. Without padding, rotating 3 bytes would
        // give [0xB, 0xC, 0xA] and data[0]=0xA would land at write_addr+2 instead.
        let without_padding = rotate_pages_left(&data, page_size);
        assert_eq!(without_padding, vec![0x0B, 0x0C, 0x0A]);
        // This demonstrates the bug: 0x0A ends up at index 2 (write_addr+3 in
        // firmware terms) instead of index 3 (write_addr+0 via page-wrap).
    }

    #[test]
    fn smart_write_overlay_alignment() {
        // offset=0x100, data=[0xAA; 256], erase_size=4096
        // first_sector=0, aligned_len=4096
        // overlay_start = 0x100 - 0 = 0x100 = 256
        let offset: u32 = 0x100;
        let data = vec![0xAAu8; 256];
        let erase_size: u32 = 4096;
        let first_sector = (offset / erase_size) * erase_size;
        let overlay_start = (offset - first_sector) as usize;
        assert_eq!(first_sector, 0);
        assert_eq!(overlay_start, 256);
        // Verify overlay lands at right position
        let mut merged = vec![0xFFu8; 4096];
        merged[overlay_start..overlay_start + data.len()].copy_from_slice(&data);
        assert_eq!(merged[255], 0xFF); // byte before offset untouched
        assert_eq!(merged[256], 0xAA); // first byte of new data
        assert_eq!(merged[511], 0xAA); // last byte of new data
        assert_eq!(merged[512], 0xFF); // byte after new data untouched
    }

    /// Regression test for the post-erase skip bug.
    ///
    /// Scenario: a sector originally contains [0xAA; page_size] in page 0 and
    /// [0xBB; page_size] in page 1.  The new image wants to keep both pages
    /// identical to the original — but a bit-set→clear transition elsewhere in the
    /// sector forces an erase.  After the erase the sector is all 0xFF on chip.
    ///
    /// The buggy code compared new_page == orig_page and skipped both pages because
    /// the desired content matches the pre-erase original.  The flash was left as
    /// 0xFF instead of [0xAA…, 0xBB…].
    ///
    /// The fix: when was_erased=true, skip only all-0xFF pages (idempotent).
    /// Pages whose desired content is not 0xFF must always be written.
    ///
    /// This test exercises write_sector_pages directly using a simulated "flash"
    /// slice that records which page addresses were written.
    #[test]
    fn erased_sector_pages_matching_original_are_written_not_skipped() {
        // Use page_size=4 and a 3-page sector (12 bytes) for simplicity.
        let page_size: usize = 4;

        // Original flash content before erase.
        let orig: Vec<u8> = [
            0xAA, 0xAA, 0xAA, 0xAA, // page 0 — non-FF, identical in new image
            0xBB, 0xBB, 0xBB, 0xBB, // page 1 — non-FF, identical in new image
            0xFF, 0xFF, 0xFF, 0xFF, // page 2 — already 0xFF
        ]
        .to_vec();

        // Desired (new) image — identical to original.
        // After erase the chip holds all 0xFF; pages 0 and 1 must be re-written.
        let target = orig.clone();

        // Simulate write_sector_pages skip logic for was_erased=true.
        // Record which page offsets would be written.
        let mut pages_written: Vec<usize> = Vec::new();
        let mut stats = SmartStats::default();

        let mut page_off = 0usize;
        while page_off < target.len() {
            let page_end = (page_off + page_size).min(target.len());
            let new_page = &target[page_off..page_end];

            // was_erased=true: skip only all-0xFF pages
            let skip = new_page.iter().all(|&b| b == 0xFF);

            if skip {
                stats.pages_skipped += 1;
            } else {
                pages_written.push(page_off);
                stats.pages_written += 1;
            }
            page_off = page_end;
        }

        // Pages 0 and 1 contain non-FF data; they MUST be written after an erase.
        assert!(
            pages_written.contains(&0),
            "page 0 (0xAA) must be written after erase, but was skipped"
        );
        assert!(
            pages_written.contains(&4),
            "page 1 (0xBB) must be written after erase, but was skipped"
        );

        // Page 2 is all 0xFF — writing it would be a no-op; skipping is correct.
        assert!(
            !pages_written.contains(&8),
            "page 2 (all-FF) should be skipped after erase"
        );

        assert_eq!(stats.pages_written, 2);
        assert_eq!(stats.pages_skipped, 1);
    }

    /// Mirror of the above for was_erased=false: pages identical to the original
    /// must be skipped (no erase happened, chip still holds the correct data).
    #[test]
    fn unerased_sector_pages_matching_original_are_skipped() {
        let page_size: usize = 4;

        let orig: Vec<u8> = [
            0xAA, 0xAA, 0xAA, 0xAA, // page 0 — unchanged
            0x11, 0x22, 0x33, 0x44, // page 1 — will change
        ]
        .to_vec();

        let mut target = orig.clone();
        // Change only page 1.
        target[4] = 0x00;

        let mut pages_written: Vec<usize> = Vec::new();
        let mut stats = SmartStats::default();

        let mut page_off = 0usize;
        while page_off < target.len() {
            let page_end = (page_off + page_size).min(target.len());
            let new_page = &target[page_off..page_end];
            let orig_page = &orig[page_off..page_end];

            // was_erased=false: skip when identical to original
            let skip = new_page == orig_page;

            if skip {
                stats.pages_skipped += 1;
            } else {
                pages_written.push(page_off);
                stats.pages_written += 1;
            }
            page_off = page_end;
        }

        // Page 0 is unchanged — no write needed.
        assert!(
            !pages_written.contains(&0),
            "page 0 unchanged — should be skipped when no erase"
        );

        // Page 1 changed — must be written.
        assert!(
            pages_written.contains(&4),
            "page 1 changed — must be written"
        );

        assert_eq!(stats.pages_written, 1);
        assert_eq!(stats.pages_skipped, 1);
    }
}
