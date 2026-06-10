//! Parallel NOR flash (EXPIO protocol), ported from FlashcatUSB PARALLEL_NOR.vb.
//! Supported on the xPort and Mach1 (the boards that route a parallel address/
//! data bus). x16 word mode only for now — covers AMD/Spansion parts like the
//! S29AL series. Read path: identify via the AMD ID sequence, then bulk-read.

use anyhow::{bail, Result};
use std::time::Duration;
use tracing::info;

use crate::progress::Progress;
use crate::usb::{UsbDevice, UsbReq};

// MEM_PROTOCOL.NOR_X16_WORD — A0..A26 = ADDR>>1, word-addressed x16 device.
const MODE_NOR_X16_WORD: u8 = 4;
// FCUSB_VPP_SET.VPP_5V (VPP pin doubles as RP# on some parts).
const VPP_5V: u32 = 1;

const READ_BLOCK: u32 = 0x10000;

/// EXPIO_INIT — configure the parallel engine for `mode`; firmware replies 0x17.
pub async fn init(dev: &UsbDevice, mode: u8) -> Result<()> {
    let r = dev.ctrl_in(UsbReq::ExpioInit, mode as u32, 1).await?;
    if r.first() != Some(&0x17) {
        bail!("EXPIO_INIT(mode {mode}) failed: {r:02x?}");
    }
    tokio::time::sleep(Duration::from_millis(50)).await;
    Ok(())
}

/// EXPIO_CTRL — set the VPP rail. pin=VPP_SET(0), cfg=None(0), opt=level.
async fn vpp_set(dev: &UsbDevice, level: u32) -> Result<()> {
    dev.ctrl_out(UsbReq::ExpioCtrl, level << 12, None).await?;
    tokio::time::sleep(Duration::from_millis(10)).await;
    Ok(())
}

/// EXPIO_WRMEMDATA — write a 16-bit word to a (big-endian) address.
async fn write_mem(dev: &UsbDevice, addr: u32, data: u16) -> Result<()> {
    let buf = [
        (addr >> 24) as u8,
        (addr >> 16) as u8,
        (addr >> 8) as u8,
        addr as u8,
        (data >> 8) as u8,
        data as u8,
    ];
    dev.ctrl_out(UsbReq::ExpioWrMemData, 0, Some(&buf)).await
}

/// EXPIO_RDMEMDATA — read a 16-bit word from an address.
async fn read_mem(dev: &UsbDevice, addr: u32) -> Result<u16> {
    let d = dev.ctrl_in(UsbReq::ExpioRdMemData, addr, 2).await?;
    if d.len() < 2 {
        bail!("short EXPIO read");
    }
    Ok(((d[1] as u16) << 8) | d[0] as u16)
}

/// Write an AMD command word. In x16 word mode the command address is shifted
/// left by one (the MCU shifts it back).
async fn write_cmd(dev: &UsbDevice, addr: u32, data: u16) -> Result<()> {
    write_mem(dev, addr << 1, data).await
}

/// Reset to read-array mode (covers AMD and Intel command sets).
async fn reset_device(dev: &UsbDevice) -> Result<()> {
    write_cmd(dev, 0x5555, 0xAA).await?;
    write_cmd(dev, 0x2AAA, 0x55).await?;
    write_cmd(dev, 0x5555, 0xF0).await?;
    write_mem(dev, 0, 0xF0).await?;
    write_mem(dev, 0, 0xFF).await?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct PnorIdent {
    pub mfg: u8,
    pub id1: u16,
    pub id2: u8,
}

/// AMD autoselect: unlock, read manufacturer + device IDs, reset.
pub async fn read_ident(dev: &UsbDevice) -> Result<PnorIdent> {
    const SHIFT: u32 = 1; // x16 word
    reset_device(dev).await?;
    tokio::time::sleep(Duration::from_millis(10)).await;
    write_cmd(dev, 0x5555, 0xAA).await?;
    write_cmd(dev, 0x2AAA, 0x55).await?;
    write_cmd(dev, 0x5555, 0x90).await?;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mfg = (read_mem(dev, 0).await? & 0xFF) as u8;
    let id1 = read_mem(dev, 1 << SHIFT).await?;
    let id2 = (read_mem(dev, 0xE << SHIFT).await? & 0xFF) as u8;
    reset_device(dev).await?;
    Ok(PnorIdent { mfg, id1, id2 })
}

/// Read CFI words at offsets 0x10.. (x16 word: address = word_offset << 1).
async fn cfi_word(dev: &UsbDevice, word_off: u32) -> Result<u8> {
    Ok((read_mem(dev, word_off << 1).await? & 0xFF) as u8)
}

/// CFI query: enter the table, read the QRY signature + device size, exit.
/// Returns `(qry_ok, size_bytes)`. Tries both entry commands the vendor uses:
/// a bare `0x98` to 0x55, then the AMD unlock prefix + `0x98`.
pub async fn read_cfi(dev: &UsbDevice) -> Result<(bool, Option<u32>)> {
    let mut qry_ok = false;
    for method in 0..2 {
        if method == 0 {
            write_cmd(dev, 0x55, 0x98).await?;
        } else {
            write_cmd(dev, 0x5555, 0xAA).await?;
            write_cmd(dev, 0x2AAA, 0x55).await?;
            write_cmd(dev, 0x5555, 0x98).await?;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
        let (q, r, y) = (
            cfi_word(dev, 0x10).await?,
            cfi_word(dev, 0x11).await?,
            cfi_word(dev, 0x12).await?,
        );
        info!("CFI method {method}: {q:#04x} {r:#04x} {y:#04x}");
        if q == 0x51 && r == 0x52 && y == 0x59 {
            qry_ok = true;
            break;
        }
        reset_device(dev).await?;
    }
    if !qry_ok {
        return Ok((false, None));
    }
    // CFI word 0x27 = device size as 2^N bytes.
    let n = cfi_word(dev, 0x27).await? as u32;
    let size = (8..=31).contains(&n).then(|| 1u32 << n);
    reset_device(dev).await?;
    Ok((true, size))
}

/// Bring up a board for parallel NOR access and identify the chip.
pub async fn setup(dev: &UsbDevice) -> Result<PnorIdent> {
    vpp_set(dev, VPP_5V).await?;
    tokio::time::sleep(Duration::from_millis(200)).await; // VCC settle (29F parts)
    init(dev, MODE_NOR_X16_WORD).await?;
    let id = read_ident(dev).await?;
    info!("parallel NOR ident: mfg={:#04x} id1={:#06x} id2={:#04x}", id.mfg, id.id1, id.id2);
    let (qry, size) = read_cfi(dev).await?;
    info!("parallel NOR CFI: QRY={qry} size={size:?}");
    Ok(id)
}

fn setup_packet(addr: u32, count: u32, page: u16) -> [u8; 20] {
    let mut b = [0u8; 20];
    b[0..4].copy_from_slice(&addr.to_le_bytes());
    b[4..8].copy_from_slice(&count.to_le_bytes());
    b[8..10].copy_from_slice(&page.to_le_bytes());
    b
}

/// Read `len` bytes from `offset` via EXPIO_READDATA, one block at a time.
pub async fn read(dev: &UsbDevice, offset: u32, len: u32) -> Result<Vec<u8>> {
    let mut pb = Progress::new("Reading (parallel)", len as u64);
    let mut out = Vec::with_capacity(len as usize);
    let mut addr = offset;
    let end = offset
        .checked_add(len)
        .ok_or_else(|| anyhow::anyhow!("read range overflows u32"))?;
    while addr < end {
        let n = READ_BLOCK.min(end - addr);
        let setup = setup_packet(addr, n, 512);
        dev.ctrl_out(UsbReq::ExpioReadData, 0, Some(&setup)).await?;
        let block = dev.bulk_in(n as usize).await?;
        if block.len() != n as usize {
            bail!("short parallel read at {addr:#x}: {} of {}", block.len(), n);
        }
        out.extend_from_slice(&block);
        addr += n;
        pb.inc(n as u64);
    }
    pb.finish();
    Ok(out)
}
