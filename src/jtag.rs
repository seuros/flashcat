//! Minimal JTAG transport and MachXO2 erase, ported from FlashcatUSB JTAG.vb
//! and the embedded MACH1_ERASE.svf. The firmware bit-bangs TMS/TDI/TCK; the
//! host only issues high-level state/shift commands over the JTAG_* requests.
//!
//! This exists to recover a Mach1 CPLD that is stuck in SPI-passthrough logic:
//! in that state the SSPI configuration port is bridged to the flash chip and
//! is unreachable, so reconfiguration must go through JTAG (dedicated pins).

use anyhow::{bail, Result};
use std::time::Duration;
use tracing::info;

use crate::usb::{UsbDevice, UsbReq};

// Firmware JTAG_MACHINE_STATE values.
const TAP_IDLE: u8 = 1;
const TAP_PAUSE_DR: u8 = 6;
const TAP_PAUSE_IR: u8 = 13;

// JTAG_SPEED enum value (not Hz): _1MHZ = 3, matching the SVF's 1 MHz.
const JTAG_SPEED_1MHZ: u32 = 3;
const MACHXO2_IDCODE: u32 = 0x012B_C043;

/// Initialise JTAG and read the TAP IDCODE via JTAG_DETECT (returns dev_count,
/// bit_length, then 4 big-endian ID bytes per device). Confirms the transport
/// works before we attempt an erase.
pub async fn detect(dev: &UsbDevice) -> Result<u32> {
    dev.ctrl_out(UsbReq::JtagInit, JTAG_SPEED_1MHZ, None).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let r = dev.ctrl_in(UsbReq::JtagDetect, 0, 64).await?;
    if r.len() < 6 || r[0] == 0 {
        bail!("JTAG: no device detected on TAP");
    }
    let id = u32::from_be_bytes([r[2], r[3], r[4], r[5]]);
    info!("JTAG: TAP IDCODE {id:#010x} ({} device(s))", r[0]);
    Ok(id)
}

async fn reset_tap(dev: &UsbDevice) -> Result<()> {
    dev.ctrl_out(UsbReq::JtagReset, 0, None).await
}

async fn goto_state(dev: &UsbDevice, state: u8) -> Result<()> {
    dev.ctrl_out(UsbReq::JtagGotoState, state as u32, None).await
}

async fn toggle(dev: &UsbDevice, ticks: u32) -> Result<()> {
    dev.ctrl_out(UsbReq::JtagToggle, ticks, None).await
}

/// Shift `bits` into IR or DR. `tdi` is MSB-first; the firmware wants LSB-first
/// byte order, so we reverse on the way in and out (mirrors ShiftIR/ShiftDR).
/// Always exits via TMS, then the caller parks the TAP in the ENDIR/ENDDR state.
async fn shift(dev: &UsbDevice, req: UsbReq, tdi: &[u8], bits: u16) -> Result<Vec<u8>> {
    let mut payload: Vec<u8> = tdi.to_vec();
    payload.reverse();
    dev.ctrl_out(UsbReq::LoadPayload, payload.len() as u32, Some(&payload))
        .await?;
    let cmd = (bits as u32) | (1 << 16); // exit_tms set
    let mut tdo = dev.ctrl_in(req, cmd, payload.len()).await?;
    tdo.reverse();
    Ok(tdo)
}

async fn shift_ir(dev: &UsbDevice, tdi: &[u8], bits: u16) -> Result<Vec<u8>> {
    let tdo = shift(dev, UsbReq::JtagShiftIr, tdi, bits).await?;
    goto_state(dev, TAP_PAUSE_IR).await?; // ENDIR IRPAUSE
    Ok(tdo)
}

async fn shift_dr(dev: &UsbDevice, tdi: &[u8], bits: u16) -> Result<Vec<u8>> {
    let tdo = shift(dev, UsbReq::JtagShiftDr, tdi, bits).await?;
    goto_state(dev, TAP_PAUSE_DR).await?; // ENDDR DRPAUSE
    Ok(tdo)
}

async fn runtest(dev: &UsbDevice, ticks: u32, secs: f64) -> Result<()> {
    goto_state(dev, TAP_IDLE).await?;
    toggle(dev, ticks).await?;
    if secs > 0.0 {
        tokio::time::sleep(Duration::from_secs_f64(secs)).await;
    }
    Ok(())
}

/// Erase the Mach1 MachXO2 via JTAG (replays MACH1_ERASE.svf). Leaves the CPLD
/// blank, so its SSPI configuration port becomes reachable again.
pub async fn mach1_erase(dev: &UsbDevice) -> Result<()> {
    info!("JTAG: erasing Mach1 CPLD");
    let id = detect(dev).await?;
    if id != MACHXO2_IDCODE {
        bail!("JTAG: unexpected TAP IDCODE {id:#010x}, expected {MACHXO2_IDCODE:#010x}");
    }
    reset_tap(dev).await?;
    goto_state(dev, TAP_IDLE).await?; // STATE IDLE

    // NB: the SVF's SAMPLE/PRELOAD step (SIR 0x1C + 552-bit SDR) is skipped —
    // this firmware stalls on control payloads larger than one 64-byte packet,
    // and the boundary-scan preload is not required for the ISC erase itself.

    // ISC_ENABLE → ISC_ERASE (first pass).
    shift_ir(dev, &[0x3C], 8).await?; // LSC_READ_STATUS
    runtest(dev, 2, 0.001).await?;
    shift_dr(dev, &[0, 0, 0, 0], 32).await?;

    shift_ir(dev, &[0xC6], 8).await?; // ISC_ENABLE
    shift_dr(dev, &[0x00], 8).await?;
    runtest(dev, 2, 0.001).await?;

    shift_ir(dev, &[0x0E], 8).await?; // ISC_ERASE
    shift_dr(dev, &[0x01], 8).await?;
    runtest(dev, 2, 1.0).await?;

    shift_ir(dev, &[0xFF], 8).await?; // BYPASS

    // ISC_ENABLE → ISC_ERASE (full erase) with busy poll.
    shift_ir(dev, &[0xC6], 8).await?; // ISC_ENABLE
    shift_dr(dev, &[0x08], 8).await?;
    runtest(dev, 2, 0.001).await?;

    shift_ir(dev, &[0x3C], 8).await?; // LSC_READ_STATUS
    runtest(dev, 2, 0.001).await?;
    shift_dr(dev, &[0, 0, 0, 0], 32).await?;

    shift_ir(dev, &[0x0E], 8).await?; // ISC_ERASE
    shift_dr(dev, &[0x0E], 8).await?;
    runtest(dev, 2, 0.0).await?;

    // LSC_CHECK_BUSY: poll the single status bit until it clears.
    shift_ir(dev, &[0xF0], 8).await?;
    let mut cleared = false;
    for i in 0..600 {
        runtest(dev, 2, 0.01).await?;
        let tdo = shift_dr(dev, &[0x00], 1).await?;
        if tdo.first().copied().unwrap_or(1) & 1 == 0 {
            info!("JTAG: erase busy cleared after {i} polls");
            cleared = true;
            break;
        }
    }
    if !cleared {
        bail!("MachXO2 JTAG erase: busy did not clear");
    }

    shift_ir(dev, &[0x3C], 8).await?; // LSC_READ_STATUS
    runtest(dev, 2, 0.001).await?;
    shift_dr(dev, &[0, 0, 0, 0], 32).await?;

    shift_ir(dev, &[0x26], 8).await?; // ISC_DISABLE
    runtest(dev, 2, 1.0).await?;
    shift_ir(dev, &[0xFF], 8).await?; // BYPASS
    runtest(dev, 100, 0.1).await?;

    reset_tap(dev).await?;
    info!("JTAG: Mach1 CPLD erase complete");
    Ok(())
}
