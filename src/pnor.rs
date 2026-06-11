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

/// EXPIO_TIMING — set parallel-bus read access and WE pulse widths (ns). The
/// vendor's defaults (200ns / 125ns) give margin on slower parts and flaky
/// adapter wiring; without it, reads can drop bits intermittently.
async fn set_timing(dev: &UsbDevice, read_access: u16, we_pulse: u16) -> Result<()> {
    let v = ((read_access as u32) << 8) | (we_pulse as u32 & 0xFF);
    dev.ctrl_out(UsbReq::ExpioTiming, v, None).await
}

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

/// CFI query: enter the table and read the device size. Returns `None` when the
/// chip has no CFI (older AMD parts like the Am29LV800B predate the standard).
pub async fn read_cfi(dev: &UsbDevice) -> Result<Option<u32>> {
    write_cmd(dev, 0x55, 0x98).await?; // enter CFI query (JEDEC: 0x98 → 0x55)
    tokio::time::sleep(Duration::from_millis(2)).await;
    let qry = cfi_word(dev, 0x10).await? == 0x51
        && cfi_word(dev, 0x11).await? == 0x52
        && cfi_word(dev, 0x12).await? == 0x59;
    let size = if qry {
        // CFI word 0x27 = device size as 2^N bytes.
        let n = cfi_word(dev, 0x27).await? as u32;
        (8..=31).contains(&n).then(|| 1u32 << n)
    } else {
        None
    };
    reset_device(dev).await?;
    Ok(size)
}

/// Result of bringing up + identifying a parallel NOR chip.
pub struct PnorChip {
    pub id: PnorIdent,
    pub name: Option<String>,
    pub size: Option<u32>,
}

/// Bring up a board for parallel NOR access and identify the chip.
pub async fn setup(dev: &UsbDevice) -> Result<PnorChip> {
    vpp_set(dev, VPP_5V).await?;
    tokio::time::sleep(Duration::from_millis(200)).await; // VCC settle (29F parts)
    init(dev, MODE_NOR_X16_WORD).await?;
    // EXPIO_TIMING is a Mach1-only feature (its FPGA has programmable bus
    // timing); the xPort's AVR uses fixed timing and STALLs on this request.
    if dev.kind == crate::programmer::Programmer::Mach1 {
        set_timing(dev, 200, 125).await?;
    }
    let id = read_ident(dev).await?;
    info!("parallel NOR ident: mfg={:#04x} id1={:#06x} id2={:#04x}", id.mfg, id.id1, id.id2);
    let db = crate::db::lookup_pnor(id.mfg, id.id1)?;
    // Prefer the DB; fall back to CFI for unlisted parts that have it.
    let size = match db {
        Some(d) => Some(d.size_bytes),
        None => read_cfi(dev).await?,
    };
    Ok(PnorChip { id, name: db.map(|d| d.name.clone()), size })
}

fn setup_packet(addr: u32, count: u32, page: u16) -> [u8; 20] {
    let mut b = [0u8; 20];
    b[0..4].copy_from_slice(&addr.to_le_bytes());
    b[4..8].copy_from_slice(&count.to_le_bytes());
    b[8..10].copy_from_slice(&page.to_le_bytes());
    b
}

/// One EXPIO_READDATA transfer of `n` bytes at `addr`.
async fn read_block_raw(dev: &UsbDevice, addr: u32, n: u32) -> Result<Vec<u8>> {
    let setup = setup_packet(addr, n, 512);
    dev.ctrl_out(UsbReq::ExpioReadData, 0, Some(&setup)).await?;
    let block = dev.bulk_in(n as usize).await?;
    if block.len() != n as usize {
        bail!("short parallel read at {addr:#x}: {} of {}", block.len(), n);
    }
    Ok(block)
}

/// Read a block until two consecutive reads agree — the parallel bus (xPort AVR
/// / marginal adapter wiring) occasionally drops a bit, so a single read isn't
/// trustworthy. Accepts the value after one matching re-read; warns if it never
/// stabilises and returns the last read.
async fn read_block_stable(dev: &UsbDevice, addr: u32, n: u32) -> Result<Vec<u8>> {
    let mut prev = read_block_raw(dev, addr, n).await?;
    for _ in 0..4 {
        let next = read_block_raw(dev, addr, n).await?;
        if next == prev {
            return Ok(next);
        }
        prev = next;
    }
    tracing::warn!("parallel read at {addr:#x} did not stabilise after retries");
    Ok(prev)
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
        out.extend_from_slice(&read_block_stable(dev, addr, n).await?);
        addr += n;
        pb.inc(n as u64);
    }
    pb.finish();
    Ok(out)
}

// E_PARALLEL_WRITEDATA.Bypass — firmware runs the AMD unlock-bypass program
// sequence (0xA0; addr=data) per word.
const WRITE_MODE_BYPASS: u32 = 3;

async fn set_write_mode(dev: &UsbDevice, mode: u32) -> Result<()> {
    dev.ctrl_out(UsbReq::ExpioModeWrite, mode, None).await
}

/// Poll GET_TASK until the firmware reports the parallel operation done.
async fn wait_task(dev: &UsbDevice) -> Result<()> {
    for _ in 0..2000 {
        tokio::time::sleep(Duration::from_millis(5)).await;
        let t = dev.ctrl_in(UsbReq::GetTask, 0, 1).await?;
        if t.first().copied().unwrap_or(0xFF) == 0 {
            return Ok(());
        }
    }
    bail!("EXPIO task timed out");
}

/// True if the 4 bytes at `addr` are all 0xFF (erased).
async fn blank_check(dev: &UsbDevice, addr: u32) -> Result<bool> {
    let d = read(dev, addr, 4).await?;
    Ok(d.iter().all(|&b| b == 0xFF))
}

/// Full-chip erase: AMD chip-erase command, then poll until blank (up to ~3 min).
pub async fn chip_erase(dev: &UsbDevice) -> Result<()> {
    info!("parallel NOR chip erase");
    write_cmd(dev, 0x5555, 0xAA).await?;
    write_cmd(dev, 0x2AAA, 0x55).await?;
    write_cmd(dev, 0x5555, 0x80).await?;
    write_cmd(dev, 0x5555, 0xAA).await?;
    write_cmd(dev, 0x2AAA, 0x55).await?;
    write_cmd(dev, 0x5555, 0x10).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    for _ in 0..360 {
        if blank_check(dev, 0).await? {
            reset_device(dev).await?;
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("chip erase timed out (not blank)");
}

async fn write_bulk(dev: &UsbDevice, addr: u32, data: &[u8]) -> Result<()> {
    let setup = setup_packet(addr, data.len() as u32, 0);
    dev.ctrl_out(UsbReq::ExpioWriteData, 0, Some(&setup)).await?;
    dev.bulk_out(data.to_vec()).await?;
    wait_task(dev).await
}

/// Program `data` at `offset` using the firmware's bypass-mode write engine.
/// The flash must already be erased (NOR can only clear bits).
pub async fn write(dev: &UsbDevice, offset: u32, data: &[u8]) -> Result<()> {
    set_write_mode(dev, WRITE_MODE_BYPASS).await?;
    let mut pb = Progress::new("Writing (parallel)", data.len() as u64);
    let mut off = 0usize;
    const CHUNK: usize = 8192;
    while off < data.len() {
        let n = CHUNK.min(data.len() - off);
        write_bulk(dev, offset + off as u32, &data[off..off + n]).await?;
        off += n;
        pb.inc(n as u64);
    }
    pb.finish();
    reset_device(dev).await?;
    Ok(())
}
