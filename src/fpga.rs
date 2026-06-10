use anyhow::{bail, Context, Result};
use tracing::{info, warn};

use crate::programmer::Programmer;
use crate::usb::{UsbDevice, UsbReq};

// One voltage-agnostic Pro PCB5 bitstream for both 1.8V and 3.3V — voltage is
// selected by the Logic1v8/Logic3v3 command, not the bitstream (source: SRC675
// FCUSBPRO_LoadBitstream loads a single "1.8V and 3V compatible" PRO5.bit).
const BITSTREAM_PRO5: &[u8] = include_bytes!("../firmware/PRO5.bit");
const BITSTREAM_MACH1_3V: &[u8] = include_bytes!("../firmware/MACH1_3V3.bit");
const BITSTREAM_MACH1_1V8: &[u8] = include_bytes!("../firmware/MACH1_1V8.bit");

const BITSTREAM_MACH1_SPI_3V: &[u8] = include_bytes!("../firmware/MACH1_SPI_3V.bit");
const BITSTREAM_MACH1_SPI_1V8: &[u8] = include_bytes!("../firmware/MACH1_SPI_1V8.bit");

// Mach1 MachXO2 stored-logic versions (source: Configuration.vb). The CPLD is
// non-volatile; if it already holds the target version we skip programming.
// SPI passthrough handles single-lane SPI; the generic FPGA bitstream carries
// the SQI engine needed for quad reads.
const MACH1_SPI_3V3: u32 = 0xAF33_0102;
const MACH1_SPI_1V8: u32 = 0xAF18_0103;
const MACH1_FGPA_3V3: u32 = 0xAF33_0007;
const MACH1_FGPA_1V8: u32 = 0xAF18_0007;

/// Whether the current operation needs the Mach1's SQI (quad) logic, which
/// lives in the generic FPGA bitstream rather than the SPI passthrough.
static MACH1_QUAD: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Select the Mach1 bitstream family for this run. Call once in `main` before
/// any `load()`; quad reads need the SQI-capable generic FPGA bitstream.
pub fn set_mach1_quad(quad: bool) {
    let _ = MACH1_QUAD.set(quad);
}

pub(crate) fn mach1_quad() -> bool {
    *MACH1_QUAD.get().unwrap_or(&false)
}

// MachXO2 SSPI configuration commands (source: ProgLogic.vb ISC_LOGIC_PROG).
const IDCODE_PUB: [u8; 4] = [0xE0, 0, 0, 0];
const ISC_ENABLE: [u8; 4] = [0xC6, 0x08, 0, 0];
const ISC_ERASE: [u8; 4] = [0x0E, 0x04, 0, 0];
const ISC_PROGRAMDONE: [u8; 4] = [0x5E, 0, 0, 0];
const LSC_INITADDRESS: [u8; 4] = [0x46, 0, 0, 0];
const LSC_PROGINCRNV: [u8; 4] = [0x70, 0, 0, 0x01];
const LSC_READ_STATUS: [u8; 4] = [0x3C, 0, 0, 0];
const LSC_REFRESH: [u8; 4] = [0x79, 0, 0, 0];
const MACHXO2_PAGE_SIZE: usize = 16;
const MACHXO2_IDCODE: u32 = 0x012B_C043;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Voltage {
    V1_8,
    V3_3,
    V5_0, // Classic only
}

/// Cut VCC to the chip socket. Safe to call after an operation completes.
///
/// On FPGA boards (Pro5/Mach1) this sends LogicOff (0xC1), which actually drops
/// the chip's VCC rail — unlike the official Windows app which deliberately
/// skips LogicOff and leaves VCC on (USB.vb:474, "skip LOGIC_OFF — resets SSPI
/// on fw 1.19"). We want VCC actually off to avoid powering the chip between
/// operations.
///
/// LogicOff resets the firmware's SSPI state machine, so the next session must
/// re-load the bitstream (load() always does this). We add a short post-op
/// settling delay so the firmware finalises its shutdown sequence before our
/// USB handle is dropped — without it, the next session sometimes hits
/// "device disconnected" on the first ctrl transfer.
pub async fn vcc_off(dev: &UsbDevice) -> Result<()> {
    info!("VCC off");
    if dev.kind.has_fpga() {
        dev.ctrl_out(UsbReq::LogicOff, 0, None).await?;
        // 100ms settling for the FPGA power rail to fully discharge and the
        // MCU firmware to commit the LogicOff state to its USB endpoints.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    // Classic / xPort: no software VCC — USB_VCC_OFF is a no-op (source: USB.vb).
    Ok(())
}

pub async fn load(dev: &UsbDevice, voltage: Voltage) -> Result<()> {
    if !dev.kind.has_fpga() {
        return Ok(());
    }

    // Mach1 uses a Lattice MachXO2 CPLD, not the Pro's iCE40 — different config
    // protocol. The iCE40 PULSE_RESET/stream path below does not apply.
    if dev.kind == Programmer::Mach1 {
        return mach1_load(dev, voltage).await;
    }

    // Do NOT send LogicOff before load — it resets SSPI (fw 1.19).
    // VCC is controlled solely by Logic3v3/Logic1v8 sent below.

    // Only the Pro reaches here; the Mach1 (MachXO2) returned via mach1_load above.
    let bitstream = match (dev.kind, voltage) {
        (Programmer::Pro5, Voltage::V5_0) => bail!("Pro does not support 5V"),
        (Programmer::Pro5, _) => BITSTREAM_PRO5,
        (Programmer::Mach1, _) => unreachable!("Mach1 handled by mach1_load"),
        (Programmer::Classic, _) => unreachable!("Classic has no FPGA"),
        (Programmer::Xport, _) => unreachable!("xPort has no FPGA"),
    };

    info!("loading FPGA bitstream ({:?} {voltage:?}, {} bytes)", dev.kind, bitstream.len());

    match voltage {
        Voltage::V3_3 => dev.ctrl_out(UsbReq::Logic3v3, 0, None).await?,
        Voltage::V1_8 => dev.ctrl_out(UsbReq::Logic1v8, 0, None).await?,
        Voltage::V5_0 => unreachable!(),
    }
    // Mirrors official USB.vb USB_VCC_ON: Sleep(100) after LOGIC_3V3 — gives the
    // FPGA's VCC rail time to ramp before we drive any SPI lines. Without this
    // delay the first SpiInit/SpiSsEnable after a prior LogicOff can fail with
    // "device disconnected".
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // SSPI init: (cs=1 << 24) | (mode=3 << 16) | speed=24
    let w32: u32 = (1u32 << 24) | (3u32 << 16) | 24u32;
    dev.ctrl_out(UsbReq::SpiInit, w32, None).await.context("SSPI_Init failed")?;

    // SS_LOW → PULSE_RESET → SS_HIGH → dummy byte → SS_LOW → bitstream → SS_HIGH → trailing clocks
    dev.ctrl_out(UsbReq::SpiSsEnable, 0, None).await?;
    dev.ctrl_out(UsbReq::PulseReset, 0, None).await?;
    dev.ctrl_out(UsbReq::SpiSsDisable, 0, None).await?;

    sspi_write(dev, &[0x00]).await?; // dummy clock

    dev.ctrl_out(UsbReq::SpiSsEnable, 0, None).await?;
    sspi_write(dev, bitstream).await.context("bitstream write failed")?;
    dev.ctrl_out(UsbReq::SpiSsDisable, 0, None).await?;

    sspi_write(dev, &[0u8; 13]).await?; // trailing clocks

    // CDONE check — fw 1.19 always returns 0 even on success; treat transport errors as fatal
    match dev.ctrl_in(UsbReq::LogicStatus, 0, 4).await {
        Ok(status) if !status.is_empty() && status[0] & 0x01 == 0 => {
            warn!("CDONE not asserted (status={:#04x}) — FPGA may not have configured correctly", status[0]);
        }
        Err(e) => return Err(e).context("LogicStatus transport error"),
        _ => {}
    }
    dev.ctrl_out(UsbReq::LogicStart, 0, None).await?;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    info!("FPGA loaded");
    Ok(())
}

async fn sspi_write(dev: &UsbDevice, data: &[u8]) -> Result<()> {
    dev.ctrl_out_nodelay(UsbReq::SpiWrData, data.len() as u32, None).await?;
    dev.bulk_out(data.to_vec()).await?;
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    Ok(())
}

/// Mach1 (MachXO2) bring-up. The CPLD is non-volatile: if it already holds the
/// SPI-passthrough bitstream we just power it, otherwise we program it.
async fn mach1_load(dev: &UsbDevice, voltage: Voltage) -> Result<()> {
    // The Mach1's generic bitstream can load for quad, but its FPGA returns
    // zeros for the assembled 4-lane data (fw 2.36) — so refuse before
    // reprogramming the CPLD. Quad works on the Pro instead.
    if mach1_quad() {
        bail!("--quad is supported on the Pro only; use a single-lane read elsewhere");
    }
    // Single-lane SPI uses the lighter passthrough bitstream.
    let (want, logic) = match (voltage, mach1_quad()) {
        (Voltage::V3_3, false) => (MACH1_SPI_3V3, BITSTREAM_MACH1_SPI_3V),
        (Voltage::V1_8, false) => (MACH1_SPI_1V8, BITSTREAM_MACH1_SPI_1V8),
        (Voltage::V3_3, true) => (MACH1_FGPA_3V3, BITSTREAM_MACH1_3V),
        (Voltage::V1_8, true) => (MACH1_FGPA_1V8, BITSTREAM_MACH1_1V8),
        (Voltage::V5_0, _) => bail!("Mach1 does not support 5V"),
    };

    // Power the CPLD rail before touching the SSPI config port (source:
    // MACH1_Init → SetDeviceVoltage runs first).
    mach1_apply_voltage(dev, voltage).await?;

    let have = dev.logic_version().await.context("reading Mach1 logic version")?;
    if have != want {
        info!("Mach1 CPLD holds {have:#010x}; programming logic {want:#010x}");
        mach1_program(dev, voltage, logic, want).await?;
        // Re-power the rail after the post-program REFRESH reboots the CPLD.
        mach1_apply_voltage(dev, voltage).await?;
    } else {
        info!("Mach1 SPI passthrough already loaded (logic {have:#010x})");
    }

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    Ok(())
}

async fn mach1_apply_voltage(dev: &UsbDevice, voltage: Voltage) -> Result<()> {
    match voltage {
        Voltage::V3_3 => dev.ctrl_out(UsbReq::Logic3v3, 0, None).await?,
        Voltage::V1_8 => dev.ctrl_out(UsbReq::Logic1v8, 0, None).await?,
        Voltage::V5_0 => bail!("Mach1 does not support 5V"),
    }
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    Ok(())
}

// --- MachXO2 SSPI configuration (port of ProgLogic.vb ISC_LOGIC_PROG) ---

/// SSPI transaction: SS low, write `cmd`, optionally read `read_len` bytes, SS high.
async fn sspi_xfer(dev: &UsbDevice, cmd: &[u8], read_len: usize) -> Result<Vec<u8>> {
    dev.ctrl_out(UsbReq::SpiSsEnable, 0, None).await?;
    if !cmd.is_empty() {
        sspi_write(dev, cmd).await?;
    }
    let out = if read_len > 0 {
        dev.ctrl_out_nodelay(UsbReq::SpiRdData, read_len as u32, None).await?;
        dev.bulk_in(read_len).await?
    } else {
        Vec::new()
    };
    dev.ctrl_out(UsbReq::SpiSsDisable, 0, None).await?;
    Ok(out)
}

/// Read the MachXO2 SSPI IDCODE, bounded by a timeout. In SPI-passthrough the
/// SSPI port is bridged to the flash and never replies, so the bulk-in would
/// hang forever; on timeout we abort the stuck endpoint and report an invalid
/// ident (0) so the caller can fall back to JTAG recovery.
async fn read_sspi_ident(dev: &UsbDevice) -> Result<u32> {
    let read = sspi_xfer(dev, &IDCODE_PUB, 4);
    match tokio::time::timeout(std::time::Duration::from_millis(1500), read).await {
        Ok(Ok(v)) if v.len() >= 4 => Ok(u32::from_be_bytes([v[0], v[1], v[2], v[3]])),
        Ok(_) => Ok(0),
        Err(_) => {
            dev.abort().await;
            Ok(0)
        }
    }
}

async fn sspi_status(dev: &UsbDevice) -> Result<u32> {
    let s = sspi_xfer(dev, &LSC_READ_STATUS, 4).await?;
    if s.len() < 4 {
        bail!("short MachXO2 status response");
    }
    Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn status_done(s: u32) -> bool { (s >> 8) & 1 == 1 }
fn status_cfg_enabled(s: u32) -> bool { (s >> 9) & 1 == 1 }
fn status_busy(s: u32) -> bool { (s >> 12) & 1 == 1 }
fn status_fail(s: u32) -> bool { (s >> 13) & 1 == 1 }
fn status_check_ok(s: u32) -> bool { (s >> 23) & 7 == 0 }

/// Poll status until the BUSY flag clears (MACHXO2_MAX_BUSY_LOOP = 128 × 10ms).
async fn sspi_wait(dev: &UsbDevice) -> Result<()> {
    for _ in 0..128 {
        let s = sspi_status(dev).await?;
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        if !status_busy(s) {
            return Ok(());
        }
    }
    bail!("MachXO2 busy-flag timeout");
}

async fn mach1_program(dev: &UsbDevice, voltage: Voltage, logic: &[u8], code: u32) -> Result<()> {
    // SSPI_Init(spi_mode=0, spi_select=1 (CS_1), speed=24)
    let w32: u32 = (1 << 24) | (0 << 16) | 24;
    dev.ctrl_out(UsbReq::SpiInit, w32, None).await?;

    let mut id = read_sspi_ident(dev).await?;
    if id != MACHXO2_IDCODE {
        // SSPI port unreachable — the CPLD is running user logic (e.g. SPI
        // passthrough bridges these pins to the flash). Recover via JTAG, which
        // uses dedicated pins, then power-cycle and re-enter SSPI. Mirrors
        // MACH1_EraseLogic (VCC off/on) + MACH1_ProgramLogic.
        info!("MachXO2 SSPI ident {id:#010x}; recovering via JTAG erase");
        crate::jtag::mach1_erase(dev).await?;
        // Power-cycle the freshly-erased CPLD so it comes up with SSPI live.
        dev.ctrl_out(UsbReq::LogicOff, 0, None).await?;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        mach1_apply_voltage(dev, voltage).await?;
        dev.ctrl_out(UsbReq::SpiInit, w32, None).await?;
        id = read_sspi_ident(dev).await?;
    }
    if id != MACHXO2_IDCODE {
        bail!("MachXO2 SSPI ident {id:#010x} != {MACHXO2_IDCODE:#010x} after JTAG erase");
    }

    if status_busy(sspi_status(dev).await?) {
        bail!("MachXO2 busy before programming");
    }

    sspi_xfer(dev, &ISC_ENABLE, 0).await?;
    let s = sspi_status(dev).await?;
    if status_fail(s) || !status_cfg_enabled(s) {
        bail!("MachXO2 ISC_ENABLE failed (status {s:#010x})");
    }

    let mut erased = false;
    for _ in 0..3 {
        sspi_xfer(dev, &ISC_ERASE, 0).await?;
        sspi_wait(dev).await?;
        let s = sspi_status(dev).await?;
        if !status_fail(s) && status_check_ok(s) {
            erased = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    if !erased {
        bail!("MachXO2 erase failed");
    }

    sspi_xfer(dev, &LSC_INITADDRESS, 0).await?;

    // Frame the bitstream: each 16-byte page is prefixed with LSC_PROGINCRNV.
    // Replicates the vendor buffer byte-for-byte (last page may be partial).
    let stride = MACHXO2_PAGE_SIZE + LSC_PROGINCRNV.len(); // 20
    let mut framed = Vec::with_capacity(logic.len() + logic.len().div_ceil(MACHXO2_PAGE_SIZE) * 4);
    for page in logic.chunks(MACHXO2_PAGE_SIZE) {
        framed.extend_from_slice(&LSC_PROGINCRNV);
        framed.extend_from_slice(page);
    }

    // Stream via SPI_REPEAT: firmware replays the buffer in `stride`-byte units.
    // setup = (stride << 16) | chunk_len; up to 128 pages per bulk transfer.
    let max_chunk = stride * 128;
    let total = framed.len();
    let mut offset = 0;
    while offset < total {
        let chunk_len = (total - offset).min(max_chunk);
        let setup = ((stride as u32) << 16) | chunk_len as u32;
        dev.ctrl_out_nodelay(UsbReq::SpiRepeat, setup, None).await?;
        dev.bulk_out(framed[offset..offset + chunk_len].to_vec()).await?;
        mach1_wait_task(dev).await?;
        offset += chunk_len;
    }

    sspi_xfer(dev, &ISC_PROGRAMDONE, 0).await?;
    sspi_wait(dev).await?;
    let s = sspi_status(dev).await?;
    if !status_done(s) {
        bail!("MachXO2 programming did not complete (status {s:#010x})");
    }

    sspi_xfer(dev, &LSC_REFRESH, 0).await?;
    let s = sspi_status(dev).await?;
    if status_busy(s) || !status_check_ok(s) {
        bail!("MachXO2 refresh failed (status {s:#010x})");
    }

    dev.logic_set_version(code).await?;
    info!("Mach1 CPLD programmed with SPI passthrough ({} bytes)", logic.len());
    Ok(())
}

/// Poll GET_TASK until the firmware reports idle (source: USB_WaitForComplete).
async fn mach1_wait_task(dev: &UsbDevice) -> Result<()> {
    for _ in 0..1000 {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let t = dev.ctrl_in(UsbReq::GetTask, 0, 1).await?;
        if t.first().copied().unwrap_or(0xFF) == 0 {
            return Ok(());
        }
    }
    bail!("MachXO2 SPI_REPEAT task timeout");
}

pub async fn set_vcc(dev: &UsbDevice, voltage: Voltage) -> Result<()> {
    if dev.kind.has_fpga() {
        // Pro/Mach1: VCC managed by Logic3v3/Logic1v8 already sent in load()
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    } else {
        // Classic / xPort have no software VCC control — target voltage is set
        // by the board's physical switch. USB_VCC_ON is a no-op on these
        // (source: USB.vb, gated by HasLogic); sending VCC_3V/5V STALLs.
        let _ = voltage;
    }
    Ok(())
}
