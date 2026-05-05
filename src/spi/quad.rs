use anyhow::{bail, Result};
use std::time::Duration;
use tracing::{debug, warn};

use crate::chip::ResolvedChip;
use crate::progress::Progress;
use crate::usb::{UsbDevice, UsbReq};

use super::bus::{ss_disable, ss_enable, spibus_read, spibus_write};
use super::write::write_enable;

// SPI_QUAD: commands sent on 1-bit, data received on 4-bit
const SPI_QUAD_IO_MODE: u32 = 3;
// Quad Output Fast Read command (1-1-4)
const CMD_QUAD_OUT_FAST_READ: u8 = 0x6B;
// Dummy clocks required by the 0x6B command
const QUAD_DUMMY_CLOCKS: u8 = 8;

const BLOCK_SIZE: u32 = 65536;
const READ_RETRIES: u32 = 3;

fn sqi_clock_div(mhz: u8) -> Result<u8> {
    match mhz {
        8  => Ok(2),
        16 => Ok(1),
        32 => Ok(0),
        _  => anyhow::bail!(
            "SQI mode supports --mhz 8, 16, or 32 only; got {mhz} \
             (24 MHz cannot be represented exactly with power-of-2 clock division)"
        ),
    }
}

/// Initialise the SQI interface on the FlashcatUSB.
/// `mhz` is the desired clock frequency; it is encoded to a divisor internally.
/// Returns an error if `mhz` is not a supported SQI clock speed (8, 16, or 32).
/// 24 MHz cannot be represented with power-of-2 division from a 32 MHz base clock.
pub async fn sqi_setup(dev: &UsbDevice, mhz: u8) -> Result<()> {
    let clock_div = sqi_clock_div(mhz)?;
    dev.ctrl_out(UsbReq::SqiSetup, clock_div.into(), None)
        .await?;
    tokio::time::sleep(Duration::from_millis(10)).await;
    Ok(())
}

const MFR_WINBOND:    u8 = 0xEF;
const MFR_GIGADEVICE: u8 = 0xC8;
const MFR_MICRON:     u8 = 0x20;
const MFR_SPANSION:   u8 = 0x01;
const MFR_ISSI:       u8 = 0x9D;
const MFR_MACRONIX:   u8 = 0xC2;
const MFR_EON:        u8 = 0x1C;

/// Enable the Quad Enable (QE) bit appropriate for the given manufacturer.
///
/// - Winbond / GigaDevice: QE = SR2[1], written with 0x31
/// - Micron N25Q / MT25QL: QE = EVCR[7] (active-low), written with 0x61
/// - Spansion S25FL: QE = CR1[1], written via 2-byte WRR (0x01 + SR1 + CR1)
/// - ISSI IS25LP: QE = Function Register[1], written with 0x42
/// - Macronix MX25L / EON EN25Q: QE = SR1[6], written with 0x01
pub async fn enable_quad(dev: &UsbDevice, mfr: u8) -> Result<()> {
    match mfr {
        MFR_WINBOND | MFR_GIGADEVICE => enable_quad_sr2(dev).await,
        MFR_MICRON                   => enable_quad_micron(dev).await,
        MFR_SPANSION                 => enable_quad_spansion(dev).await,
        MFR_ISSI                     => enable_quad_issi(dev).await,
        MFR_MACRONIX | MFR_EON       => enable_quad_sr1(dev).await,
        mfr => bail!(
            "quad enable not supported for unknown manufacturer {mfr:#04x} — \
             SR1 QE bit location is not confirmed; use a known chip or check the datasheet"
        ),
    }
}

/// W25Q / GD25Q: QE is SR2[1]. Use 0x35 to read, 0x31 to write.
async fn enable_quad_sr2(dev: &UsbDevice) -> Result<()> {
    debug!("enabling QE via SR2[1] (Winbond/GigaDevice path)");

    ss_enable(dev).await?;
    let wr        = spibus_write(dev, &[0x35]).await; // RDSR2
    let rd        = spibus_read(dev, 1).await;
    let dis       = ss_disable(dev).await;
    wr?;
    let sr2_bytes = rd?;
    dis?;

    let sr2 = sr2_bytes.first().copied().unwrap_or(0);
    if sr2 & 0x02 != 0 {
        debug!("QE already set (SR2={sr2:#04x}), skipping");
        return Ok(());
    }

    let new_sr2 = sr2 | 0x02;
    debug!("SR2 before={sr2:#04x} after={new_sr2:#04x}");

    ss_enable(dev).await?;
    let wr  = spibus_write(dev, &[0x06]).await; // WREN
    let dis = ss_disable(dev).await;
    wr?;
    dis?;

    ss_enable(dev).await?;
    let wr  = spibus_write(dev, &[0x31, new_sr2]).await; // WRSR2
    let dis = ss_disable(dev).await;
    wr?;
    dis?;

    poll_wip(dev, "WRSR2").await
}

/// Macronix MX25L / EON EN25Q: QE is SR1[6]. Use 0x05 to read SR1, 0x01 to write.
/// Many MX25L parts require a 2-byte WRSR: [0x01, SR1, CR1]. Sending only 1 byte
/// zeros CR1. Read CR1 first (RDCR = 0x15) and preserve it in the write.
async fn enable_quad_sr1(dev: &UsbDevice) -> Result<()> {
    debug!("enabling QE via SR1[6] (Macronix/EON path)");

    // Read SR1
    ss_enable(dev).await?;
    let wr      = spibus_write(dev, &[0x05]).await; // RDSR
    let rd      = spibus_read(dev, 1).await;
    let dis     = ss_disable(dev).await;
    wr?;
    let sr_bytes = rd?;
    dis?;

    let sr = sr_bytes.first().copied().unwrap_or(0);
    if sr & 0x40 != 0 {
        debug!("QE already set (SR={sr:#04x}), skipping");
        return Ok(());
    }

    // Read CR1 (RDCR = 0x15) to preserve its current value
    ss_enable(dev).await?;
    let wr       = spibus_write(dev, &[0x15]).await; // RDCR
    let rd       = spibus_read(dev, 1).await;
    let dis      = ss_disable(dev).await;
    wr?;
    let cr1_data = rd?;
    dis?;
    let cr1 = cr1_data.first().copied().unwrap_or(0);

    let new_sr = sr | 0x40;
    debug!("SR before={sr:#04x} after={new_sr:#04x} CR1={cr1:#04x} (preserved)");

    write_enable(dev).await?;
    ss_enable(dev).await?;
    let wr  = spibus_write(dev, &[0x01, new_sr, cr1]).await; // WRSR: SR1 + CR1
    let dis = ss_disable(dev).await;
    wr?;
    dis?;

    poll_wip(dev, "WRSR").await
}

/// Micron N25Q / MT25QL: QE is EVCR[7], active-low (0 = quad enabled).
/// Read with 0x65 (RDEVCR), write with 0x61 (WEVCR). Volatile — no erase cycle.
async fn enable_quad_micron(dev: &UsbDevice) -> Result<()> {
    debug!("enabling QE via EVCR[7] (Micron path)");

    ss_enable(dev).await?;
    let wr         = spibus_write(dev, &[0x65]).await; // RDEVCR
    let rd         = spibus_read(dev, 1).await;
    let dis        = ss_disable(dev).await;
    wr?;
    let evcr_bytes = rd?;
    dis?;

    let evcr = evcr_bytes.first().copied().unwrap_or(0xFF);
    if evcr & 0x80 == 0 {
        debug!("EVCR quad already enabled (EVCR={evcr:#04x}), skipping");
        return Ok(());
    }

    let new_evcr = evcr & !0x80;
    debug!("EVCR before={evcr:#04x} after={new_evcr:#04x}");

    // WEVCR (0x61) is a volatile write; WREN is not required and must not be
    // issued — some parts interpret WREN+WEVCR as a nonvolatile write sequence.
    ss_enable(dev).await?;
    let wr  = spibus_write(dev, &[0x61, new_evcr]).await; // WEVCR (volatile, no WREN)
    let dis = ss_disable(dev).await;
    wr?;
    dis?;

    poll_wip(dev, "WEVCR").await
}

/// Spansion S25FL: QE is CR1[1]. WRR (0x01) takes SR1 + CR1 as two bytes.
/// Read SR1 with 0x05, read CR1 with 0x35, write both atomically with 0x01.
async fn enable_quad_spansion(dev: &UsbDevice) -> Result<()> {
    debug!("enabling QE via CR1[1] (Spansion path)");

    ss_enable(dev).await?;
    let wr        = spibus_write(dev, &[0x05]).await; // RDSR1
    let rd        = spibus_read(dev, 1).await;
    let dis       = ss_disable(dev).await;
    wr?;
    let sr1_bytes = rd?;
    dis?;

    ss_enable(dev).await?;
    let wr        = spibus_write(dev, &[0x35]).await; // RDCR1
    let rd        = spibus_read(dev, 1).await;
    let dis       = ss_disable(dev).await;
    wr?;
    let cr1_bytes = rd?;
    dis?;

    let sr1 = sr1_bytes.first().copied().unwrap_or(0);
    let cr1 = cr1_bytes.first().copied().unwrap_or(0);

    if cr1 & 0x02 != 0 {
        debug!("QE already set (CR1={cr1:#04x}), skipping");
        return Ok(());
    }

    let new_cr1 = cr1 | 0x02;
    debug!("CR1 before={cr1:#04x} after={new_cr1:#04x}");

    ss_enable(dev).await?;
    let wr  = spibus_write(dev, &[0x06]).await; // WREN
    let dis = ss_disable(dev).await;
    wr?;
    dis?;

    ss_enable(dev).await?;
    let wr  = spibus_write(dev, &[0x01, sr1, new_cr1]).await; // WRR: SR1 + CR1
    let dis = ss_disable(dev).await;
    wr?;
    dis?;

    poll_wip(dev, "WRR").await
}

/// ISSI IS25LP: QE is Function Register[1].
/// Read with 0x48 (RDFR), write with 0x42 (WRFR).
async fn enable_quad_issi(dev: &UsbDevice) -> Result<()> {
    debug!("enabling QE via FR[1] (ISSI path)");

    ss_enable(dev).await?;
    let wr      = spibus_write(dev, &[0x48]).await; // RDFR
    let rd      = spibus_read(dev, 1).await;
    let dis     = ss_disable(dev).await;
    wr?;
    let fr_bytes = rd?;
    dis?;

    let fr = fr_bytes.first().copied().unwrap_or(0);
    if fr & 0x02 != 0 {
        debug!("QE already set (FR={fr:#04x}), skipping");
        return Ok(());
    }

    let new_fr = fr | 0x02;
    debug!("FR before={fr:#04x} after={new_fr:#04x}");

    ss_enable(dev).await?;
    let wr  = spibus_write(dev, &[0x06]).await; // WREN
    let dis = ss_disable(dev).await;
    wr?;
    dis?;

    ss_enable(dev).await?;
    let wr  = spibus_write(dev, &[0x42, new_fr]).await; // WRFR
    let dis = ss_disable(dev).await;
    wr?;
    dis?;

    poll_wip(dev, "WRFR").await
}

/// Poll SR1 WIP bit until the register write completes (up to ~700 ms).
/// Spansion nonvolatile SR writes can take up to 600 ms; 140 × 5 ms = 700 ms.
async fn poll_wip(dev: &UsbDevice, op: &str) -> Result<()> {
    for _ in 0..140 {
        tokio::time::sleep(Duration::from_millis(5)).await;
        ss_enable(dev).await?;
        let wr   = spibus_write(dev, &[0x05]).await; // RDSR
        let rd   = spibus_read(dev, 1).await;
        let dis  = ss_disable(dev).await;
        wr?;
        let poll = rd?;
        dis?;
        if poll.first().map(|b| b & 0x01).unwrap_or(1) == 0 {
            debug!("QE enabled after {op}");
            return Ok(());
        }
    }
    bail!("timeout waiting for {op} to complete");
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
    let end = offset.checked_add(length)
        .ok_or_else(|| anyhow::anyhow!("quad read range overflows u32: offset={offset:#x} length={length:#x}"))?;

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
        assert_eq!(sqi_clock_div(8).unwrap(),  2);
        assert_eq!(sqi_clock_div(16).unwrap(), 1);
        assert_eq!(sqi_clock_div(32).unwrap(), 0);
    }

    #[test]
    fn test_clock_div_rejects_unsupported_speeds() {
        assert!(sqi_clock_div(1).is_err());
        assert!(sqi_clock_div(2).is_err());
        assert!(sqi_clock_div(4).is_err());
        assert!(sqi_clock_div(12).is_err());
        // 24 MHz cannot be represented with power-of-2 division; must be rejected
        assert!(sqi_clock_div(24).is_err());
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
