use anyhow::{bail, Result};
use std::time::Duration;
use tracing::{debug, warn};

use crate::chip::ResolvedChip;
use crate::progress::Progress;
use crate::usb::{UsbDevice, UsbReq};

use super::bus::{ss_disable, ss_enable, spibus_read, spibus_write};

// SPI_QUAD: commands sent on 1-bit, data received on 4-bit
const SPI_QUAD_IO_MODE: u32 = 3;
// Quad Output Fast Read command (1-1-4)
const CMD_QUAD_OUT_FAST_READ: u8 = 0x6B;
// Dummy clocks required by the 0x6B command
const QUAD_DUMMY_CLOCKS: u8 = 8;

const BLOCK_SIZE: u32 = 65536;
const READ_RETRIES: u32 = 3;

fn sqi_clock_div(mhz: u8) -> u32 {
    match mhz {
        8  => 0,
        16 => 1,
        24 => 2,
        32 => 3,
        _  => 4,
    }
}

/// Initialise the SQI interface on the FlashcatUSB.
/// `mhz` is the desired clock frequency; it is encoded to a divisor internally.
pub async fn sqi_setup(dev: &UsbDevice, mhz: u8) -> Result<()> {
    let clock_div = sqi_clock_div(mhz);
    dev.ctrl_out(UsbReq::SqiSetup, clock_div, None)
        .await?;
    tokio::time::sleep(Duration::from_millis(10)).await;
    Ok(())
}

/// Enable the Quad Enable (QE) bit in the chip's status register.
///
/// Sequence: RDSR (0x05) → set bit 6 → WREN (0x06) → WRSR (0x01, new_sr).
pub async fn enable_quad(dev: &UsbDevice) -> Result<()> {
    debug!("enabling QE bit in status register");

    // Read current status register
    ss_enable(dev).await?;
    spibus_write(dev, &[0x05]).await?; // RDSR
    let sr_bytes = spibus_read(dev, 1).await?;
    ss_disable(dev).await?;

    let sr = sr_bytes.first().copied().unwrap_or(0);
    if sr & 0x40 != 0 {
        debug!("QE bit already set (SR={sr:#04x}), skipping");
        return Ok(());
    }

    let new_sr = sr | 0x40;
    debug!("SR before={sr:#04x} after={new_sr:#04x}");

    // Write enable
    ss_enable(dev).await?;
    spibus_write(dev, &[0x06]).await?; // WREN
    ss_disable(dev).await?;

    // Write status register with QE bit set
    ss_enable(dev).await?;
    spibus_write(dev, &[0x01, new_sr]).await?; // WRSR
    ss_disable(dev).await?;

    // Poll WIP until write completes (SR write can take ~15 ms)
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(5)).await;
        ss_enable(dev).await?;
        spibus_write(dev, &[0x05]).await?; // RDSR
        let poll = spibus_read(dev, 1).await?;
        ss_disable(dev).await?;
        if poll.first().map(|b| b & 0x01).unwrap_or(1) == 0 {
            debug!("QE bit enabled (SR={:#04x})", poll[0]);
            return Ok(());
        }
    }

    bail!("timeout waiting for WRSR to complete");
}

/// Read `length` bytes starting at `offset` using the SQI Quad Output Fast Read path.
pub async fn read_quad(
    dev: &UsbDevice,
    chip: &ResolvedChip,
    offset: u32,
    length: u32,
) -> Result<Vec<u8>> {
    let mut pb = Progress::new("Reading (Quad)", length as u64);
    let mut out = Vec::with_capacity(length as usize);
    let mut addr = offset;
    let end = offset + length;

    while addr < end {
        let block = BLOCK_SIZE.min(end - addr);
        let data = read_quad_block(dev, chip, addr, block).await?;
        out.extend_from_slice(&data);
        addr += block;
        pb.inc(block as u64);
    }

    pb.finish();
    Ok(out)
}

async fn read_quad_block(
    dev: &UsbDevice,
    chip: &ResolvedChip,
    addr: u32,
    len: u32,
) -> Result<Vec<u8>> {
    for attempt in 0..READ_RETRIES {
        match try_read_quad_block(dev, chip, addr, len).await {
            Ok(data) => return Ok(data),
            Err(e) => {
                warn!("quad read block {addr:#010x} attempt {attempt}: {e}");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
    bail!("quad read block {addr:#010x} failed after {READ_RETRIES} attempts");
}

async fn try_read_quad_block(
    dev: &UsbDevice,
    chip: &ResolvedChip,
    addr: u32,
    len: u32,
) -> Result<Vec<u8>> {
    debug!("SQI_RDFLASH addr={addr:#010x} len={len}");
    let setup = sqi_read_setup_packet(chip.addr_bytes, addr, len);
    dev.ctrl_out(UsbReq::SqiRdFlash, 0, Some(&setup)).await?;
    let result = dev.bulk_in(len as usize).await?;
    if result.len() != len as usize {
        bail!("short quad read: {} of {}", result.len(), len);
    }
    Ok(result)
}

/// Build the 11-byte SQI ReadSetupPacket.
///
/// Layout mirrors the SPI ReadSetupPacket but byte[9] carries dummy clocks
/// and byte[10] carries io_mode (3 = SPI_QUAD).
fn sqi_read_setup_packet(addr_bytes: u8, offset: u32, count: u32) -> [u8; 11] {
    [
        CMD_QUAD_OUT_FAST_READ,
        addr_bytes,
        ((offset >> 24) & 0xFF) as u8,
        ((offset >> 16) & 0xFF) as u8,
        ((offset >> 8)  & 0xFF) as u8,
        (offset         & 0xFF) as u8,
        ((count  >> 16) & 0xFF) as u8,
        ((count  >> 8)  & 0xFF) as u8,
        (count          & 0xFF) as u8,
        QUAD_DUMMY_CLOCKS,
        SPI_QUAD_IO_MODE as u8,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clock_div_encoding() {
        assert_eq!(sqi_clock_div(8),  0);
        assert_eq!(sqi_clock_div(16), 1);
        assert_eq!(sqi_clock_div(24), 2);
        assert_eq!(sqi_clock_div(32), 3);
        assert_eq!(sqi_clock_div(1),  4);
        assert_eq!(sqi_clock_div(12), 4);
    }

    #[test]
    fn test_sqi_read_setup_packet() {
        let pkt = sqi_read_setup_packet(3, 0x00ABC000, 0x10000);
        assert_eq!(pkt[0],  0x6B);          // CMD_QUAD_OUT_FAST_READ
        assert_eq!(pkt[1],  3);             // addr_bytes
        assert_eq!(pkt[2],  0x00);          // offset[31:24]
        assert_eq!(pkt[3],  0xAB);          // offset[23:16]
        assert_eq!(pkt[4],  0xC0);          // offset[15:8]
        assert_eq!(pkt[5],  0x00);          // offset[7:0]
        assert_eq!(pkt[6],  0x01);          // count[23:16]
        assert_eq!(pkt[7],  0x00);          // count[15:8]
        assert_eq!(pkt[8],  0x00);          // count[7:0]
        assert_eq!(pkt[9],  8);             // dummy clocks
        assert_eq!(pkt[10], 3);             // SPI_QUAD io_mode
    }

    #[test]
    fn test_sqi_setup_packet_length() {
        let pkt = sqi_read_setup_packet(3, 0, 1);
        assert_eq!(pkt.len(), 11);
    }
}
