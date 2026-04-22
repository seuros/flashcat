use anyhow::{bail, Result};
use std::time::Duration;
use tracing::{debug, warn};

use crate::db::SpiNorDef;
use crate::progress::Progress;
use crate::usb::{UsbDevice, UsbReq};

const BLOCK_SIZE: u32 = 65536;
const READ_RETRIES: u32 = 3;

pub async fn read(dev: &UsbDevice, chip: &SpiNorDef, offset: u32, length: u32) -> Result<Vec<u8>> {
    let mut pb = Progress::new("Reading", length as u64);
    let mut out = Vec::with_capacity(length as usize);
    let mut addr = offset;
    let end = offset + length;

    while addr < end {
        let block = BLOCK_SIZE.min(end - addr);
        let data = read_block(dev, chip, addr, block).await?;
        out.extend_from_slice(&data);
        addr += block;
        pb.inc(block as u64);
    }

    pb.finish();
    Ok(out)
}

async fn read_block(dev: &UsbDevice, chip: &SpiNorDef, addr: u32, len: u32) -> Result<Vec<u8>> {
    for attempt in 0..READ_RETRIES {
        match try_read_block(dev, chip, addr, len).await {
            Ok(data) => return Ok(data),
            Err(e) => {
                warn!("read block {addr:#010x} attempt {attempt}: {e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    bail!("read block {addr:#010x} failed after {READ_RETRIES} attempts");
}

pub(crate) async fn try_read_block(
    dev: &UsbDevice,
    chip: &SpiNorDef,
    addr: u32,
    len: u32,
) -> Result<Vec<u8>> {
    debug!("SPI_READFLASH addr={addr:#010x} len={len}");
    // ReadSetupPacket (11 bytes) sent as ctrl_out payload, then bulk_in for data
    let setup = read_setup_packet(0x03, chip.addr_bytes, addr, len, 0);
    dev.ctrl_out(UsbReq::SpiReadFlash, 0, Some(&setup)).await?;
    let result = dev.bulk_in(len as usize).await?;

    if result.len() != len as usize {
        bail!("short read: {} of {}", result.len(), len);
    }
    Ok(result)
}

pub(crate) fn read_setup_packet(
    cmd: u8,
    addr_bytes: u8,
    offset: u32,
    count: u32,
    dummy: u8,
) -> [u8; 11] {
    [
        cmd,
        addr_bytes,
        ((offset >> 24) & 0xFF) as u8,
        ((offset >> 16) & 0xFF) as u8,
        ((offset >> 8) & 0xFF) as u8,
        (offset & 0xFF) as u8,
        ((count >> 16) & 0xFF) as u8,
        ((count >> 8) & 0xFF) as u8,
        (count & 0xFF) as u8,
        dummy,
        0, // SPI_ONLY
    ]
}
