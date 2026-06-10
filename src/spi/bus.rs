use anyhow::Result;
use std::time::Duration;

use crate::programmer::Programmer;
use crate::usb::{UsbDevice, UsbReq};

// MULTI_IO_MODE.Single — 1 bit/clock. The SQI engine in single mode is a drop-in
// for plain SPI, used for command/setup access (RDID, status, QE) in quad mode.
const IO_SINGLE: u32 = 1;

/// In quad mode the FPGA boards route flash access through the SQI command
/// group (0x51-0x54) instead of the SPI group (0x41-0x45). The Mach1 loads its
/// generic bitstream for this; the Pro uses its single bitstream for both.
pub(crate) fn use_sqi(dev: &UsbDevice) -> bool {
    crate::fpga::mach1_quad()
        && matches!(
            dev.kind,
            Programmer::Mach1 | Programmer::Pro5 | Programmer::Xport
        )
}

const CMD_DEEP_POWER_DOWN: u8 = 0xB9;
const CMD_RELEASE_DEEP_POWER_DOWN: u8 = 0xAB;
const CMD_READ_STATUS: u8 = 0x05;
const STATUS_WIP: u8 = 0x01;
const DEEP_POWER_DOWN_ENTRY_DELAY: Duration = Duration::from_millis(1);
const DEEP_POWER_DOWN_READY_POLL_DELAY: Duration = Duration::from_millis(5);
const DEEP_POWER_DOWN_READY_POLLS: u8 = 20;
// tRES1 (release-from-DPD to standby) is ~30 us typ for N25Q; 1 ms covers all parts.
const RELEASE_DEEP_POWER_DOWN_DELAY: Duration = Duration::from_millis(1);

// {SPI,SQI}_WR_DATA: ctrl_out arms transfer, bulk_out must follow without delay.
// SQI write encodes the IO mode in the high byte of the control value.
pub(crate) async fn spibus_write(dev: &UsbDevice, data: &[u8]) -> Result<()> {
    if use_sqi(dev) {
        let v = (IO_SINGLE << 24) | (data.len() as u32 & 0x00FF_FFFF);
        dev.ctrl_out_nodelay(UsbReq::SqiWrData, v, None).await?;
    } else {
        dev.ctrl_out_nodelay(UsbReq::SpiWrData, data.len() as u32, None).await?;
    }
    dev.bulk_out(data.to_vec()).await
}

// {SPI,SQI}_RD_DATA: ctrl_out arms transfer, bulk_in must follow without delay.
pub(crate) async fn spibus_read(dev: &UsbDevice, len: usize) -> Result<Vec<u8>> {
    if use_sqi(dev) {
        let v = (IO_SINGLE << 24) | (len as u32 & 0x00FF_FFFF);
        dev.ctrl_out_nodelay(UsbReq::SqiRdData, v, None).await?;
    } else {
        dev.ctrl_out_nodelay(UsbReq::SpiRdData, len as u32, None).await?;
    }
    dev.bulk_in(len).await
}

pub(crate) async fn ss_enable(dev: &UsbDevice) -> Result<()> {
    let req = if use_sqi(dev) { UsbReq::SqiSsEnable } else { UsbReq::SpiSsEnable };
    dev.ctrl_out(req, 0, None).await
}

pub(crate) async fn ss_disable(dev: &UsbDevice) -> Result<()> {
    let req = if use_sqi(dev) { UsbReq::SqiSsDisable } else { UsbReq::SpiSsDisable };
    dev.ctrl_out(req, 0, None).await
}

fn deep_power_down_packet() -> [u8; 1] {
    [CMD_DEEP_POWER_DOWN]
}

async fn read_status(dev: &UsbDevice) -> Result<u8> {
    ss_enable(dev).await?;
    let write_result = spibus_write(dev, &[CMD_READ_STATUS]).await;
    let read_result = if write_result.is_ok() {
        Some(spibus_read(dev, 1).await)
    } else {
        None
    };
    let disable_result = ss_disable(dev).await;

    write_result?;
    let status_result = read_result
        .expect("status read is attempted when command write succeeds");
    disable_result?;
    let status = status_result?
        .first()
        .copied()
        .unwrap_or(STATUS_WIP);
    Ok(status)
}

async fn wait_ready_for_deep_power_down(dev: &UsbDevice) -> Result<()> {
    for _ in 0..DEEP_POWER_DOWN_READY_POLLS {
        if read_status(dev).await? & STATUS_WIP == 0 {
            return Ok(());
        }
        tokio::time::sleep(DEEP_POWER_DOWN_READY_POLL_DELAY).await;
    }

    anyhow::bail!("flash still reports WIP before deep power-down");
}

/// Send Deep Power-Down (0xB9) so the chip draws ~1 uA before VCC is cut.
/// Best-effort cleanup; the chip may not respond if already in a bad state.
pub(crate) async fn deep_power_down(dev: &UsbDevice) -> Result<()> {
    wait_ready_for_deep_power_down(dev).await?;

    ss_enable(dev).await?;
    let write_result = spibus_write(dev, &deep_power_down_packet()).await;
    let disable_result = ss_disable(dev).await;

    write_result?;
    disable_result?;

    tokio::time::sleep(DEEP_POWER_DOWN_ENTRY_DELAY).await;
    Ok(())
}

/// Send Release from Deep Power-Down (0xAB) to wake a chip that a prior session
/// left in DPD via `deep_power_down()` (0xB9). A chip in DPD ignores every
/// command *except* 0xAB — software reset (0x66/0x99) and RDID (0x9F) all read
/// blank — so without this, back-to-back invocations report "no chip detected"
/// whenever residual clip/IO power kept the part asleep across the VCC cut.
/// Harmless no-op if the chip is already awake.
pub(crate) async fn release_deep_power_down(dev: &UsbDevice) -> Result<()> {
    ss_enable(dev).await?;
    let write_result = spibus_write(dev, &[CMD_RELEASE_DEEP_POWER_DOWN]).await;
    let disable_result = ss_disable(dev).await;

    write_result?;
    disable_result?;

    tokio::time::sleep(RELEASE_DEEP_POWER_DOWN_DELAY).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deep_power_down_packet() {
        assert_eq!(deep_power_down_packet(), [0xB9]);
    }
}
