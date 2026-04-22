use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::usb::{UsbDevice, UsbReq};

const BITSTREAM_PRO_3V: &[u8] = include_bytes!("../firmware/PRO5_3V.bit");
const BITSTREAM_PRO_1V8: &[u8] = include_bytes!("../firmware/PRO5_1V8.bit");

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Voltage {
    V1_8,
    V3_3,
}

pub async fn load(dev: &UsbDevice, voltage: Voltage) -> Result<()> {
    if !dev.has_logic {
        return Ok(());
    }

    let bitstream = match voltage {
        Voltage::V3_3 => BITSTREAM_PRO_3V,
        Voltage::V1_8 => BITSTREAM_PRO_1V8,
    };

    info!("loading FPGA bitstream ({voltage:?}, {} bytes)", bitstream.len());

    // set logic voltage (also powers the iCE40)
    match voltage {
        Voltage::V3_3 => dev.ctrl_out(UsbReq::Logic3v3, 0, None).await?,
        Voltage::V1_8 => dev.ctrl_out(UsbReq::Logic1v8, 0, None).await?,
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
        Ok(status) if !status.is_empty() && status[0] & 0x01 != 0 => {
            warn!("CDONE not asserted (status={:#04x}) — continuing anyway", status[0]);
        }
        Err(e) => return Err(e).context("LogicStatus transport error"),
        _ => {}
    }
    dev.ctrl_out(UsbReq::LogicStart, 0, None).await?;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    info!("FPGA loaded");
    Ok(())
}

// SSPI_WriteData: ctrl_out(SPI_WR_DATA, len) then bulk_out(data)
async fn sspi_write(dev: &UsbDevice, data: &[u8]) -> Result<()> {
    dev.ctrl_out(UsbReq::SpiWrData, data.len() as u32, None).await?;
    dev.bulk_out(data.to_vec()).await?;
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    Ok(())
}

pub async fn set_vcc(dev: &UsbDevice, voltage: Voltage) -> Result<()> {
    if dev.has_logic {
        // Pro/Mach1: VCC is managed by LOGIC_3V3/LOGIC_1V8 (already sent in load())
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    } else {
        // Classic: separate VCC control
        match voltage {
            Voltage::V3_3 => dev.ctrl_out(UsbReq::Vcc3v, 0, None).await?,
            Voltage::V1_8 => dev.ctrl_out(UsbReq::Vcc1v8, 0, None).await?,
        }
        dev.ctrl_out(UsbReq::VccOn, 0, None).await?;
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    Ok(())
}
