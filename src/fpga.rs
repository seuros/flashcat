use anyhow::{bail, Context, Result};
use tracing::{info, warn};

use crate::programmer::Programmer;
use crate::usb::{UsbDevice, UsbReq};

const BITSTREAM_PRO5_3V: &[u8] = include_bytes!("../firmware/PRO5_3V.bit");
const BITSTREAM_PRO5_1V8: &[u8] = include_bytes!("../firmware/PRO5_1V8.bit");
const BITSTREAM_MACH1_3V: &[u8] = include_bytes!("../firmware/MACH1_3V3.bit");
const BITSTREAM_MACH1_1V8: &[u8] = include_bytes!("../firmware/MACH1_1V8.bit");

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Voltage {
    V1_8,
    V3_3,
    V5_0, // Classic only
}

/// Cut VCC to the chip socket. Safe to call after an operation completes.
/// LogicOff resets the SSPI interface (fw 1.19 note), but load() always
/// reinitialises SSPI before the next bitstream, so this is safe post-op.
pub async fn vcc_off(dev: &UsbDevice) -> Result<()> {
    if dev.kind.has_fpga() {
        dev.ctrl_out(UsbReq::LogicOff, 0, None).await?;
    } else {
        dev.ctrl_out(UsbReq::VccOff, 0, None).await?;
    }
    Ok(())
}

pub async fn load(dev: &UsbDevice, voltage: Voltage) -> Result<()> {
    if !dev.kind.has_fpga() {
        return Ok(());
    }

    // Do NOT send LogicOff before load — it resets SSPI (fw 1.19).
    // VCC is controlled solely by Logic3v3/Logic1v8 sent below.

    let bitstream = match (dev.kind, voltage) {
        (Programmer::Pro5,  Voltage::V3_3) => BITSTREAM_PRO5_3V,
        (Programmer::Pro5,  Voltage::V1_8) => BITSTREAM_PRO5_1V8,
        (Programmer::Mach1, Voltage::V3_3) => BITSTREAM_MACH1_3V,
        (Programmer::Mach1, Voltage::V1_8) => BITSTREAM_MACH1_1V8,
        (_, Voltage::V5_0) => bail!("FPGA programmers do not support 5V"),
        (Programmer::Classic, _) => unreachable!("Classic has no FPGA"),
    };

    info!("loading FPGA bitstream ({:?} {voltage:?}, {} bytes)", dev.kind, bitstream.len());

    match voltage {
        Voltage::V3_3 => dev.ctrl_out(UsbReq::Logic3v3, 0, None).await?,
        Voltage::V1_8 => dev.ctrl_out(UsbReq::Logic1v8, 0, None).await?,
        Voltage::V5_0 => unreachable!(),
    }

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

pub async fn set_vcc(dev: &UsbDevice, voltage: Voltage) -> Result<()> {
    if dev.kind.has_fpga() {
        // Pro/Mach1: VCC managed by Logic3v3/Logic1v8 already sent in load()
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    } else {
        // Classic: separate VCC control; supports 3.3V and 5V
        match voltage {
            Voltage::V3_3 => dev.ctrl_out(UsbReq::Vcc3v, 0, None).await?,
            Voltage::V5_0 => dev.ctrl_out(UsbReq::Vcc5v, 0, None).await?,
            Voltage::V1_8 => bail!("Classic does not support 1.8V"),
        }
        dev.ctrl_out(UsbReq::VccOn, 0, None).await?;
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    Ok(())
}
