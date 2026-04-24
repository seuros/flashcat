use anyhow::{bail, Result};
use std::time::Duration;

use crate::chip::ResolvedChip;
use crate::progress::Progress;
use crate::usb::{UsbDevice, UsbReq};

use super::bus::{spibus_read, spibus_write, ss_disable, ss_enable};

pub async fn write(dev: &UsbDevice, chip: &ResolvedChip, offset: u32, data: &[u8]) -> Result<()> {
    let mut pb = Progress::new("Writing", data.len() as u64);
    let mut addr = offset;
    let mut remaining = data;

    while !remaining.is_empty() {
        let page_offset = addr % chip.page_size;
        let chunk_size = (chip.page_size - page_offset).min(remaining.len() as u32) as usize;
        let (chunk, rest) = remaining.split_at(chunk_size);

        write_page(dev, chip, addr, chunk).await?;
        addr += chunk_size as u32;
        remaining = rest;
        pb.inc(chunk_size as u64);
    }

    pb.finish();
    Ok(())
}

async fn write_page(dev: &UsbDevice, chip: &ResolvedChip, addr: u32, data: &[u8]) -> Result<()> {
    // ctrl_out arms the firmware, bulk_out must follow without delay
    let setup = write_setup_packet(chip, addr, data.len() as u32);
    dev.ctrl_out_nodelay(UsbReq::SpiWriteFlash, 0, Some(&setup)).await?;
    dev.bulk_out(data.to_vec()).await?;
    wait_wip(dev).await
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
    poll_wip(dev, 200, 10).await // 2s — page program / sector erase
}

pub(crate) async fn wait_wip_block(dev: &UsbDevice) -> Result<()> {
    poll_wip(dev, 500, 10).await // 5s — 64KB block erase
}

pub(crate) async fn wait_wip_long(dev: &UsbDevice) -> Result<()> {
    poll_wip(dev, 20_000, 10).await // 200s — chip erase
}
