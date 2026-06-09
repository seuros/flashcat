use anyhow::{bail, Context, Result};
use nusb::{
    transfer::{Bulk, Buffer, ControlIn, ControlOut, ControlType, In, Out, Recipient},
    Interface,
};
use std::time::Duration;
use tracing::debug;

use super::requests::UsbReq;
use crate::programmer::Programmer;

const TIMEOUT: Duration = Duration::from_millis(5000);

// EP1 IN = 0x81, EP2 OUT = 0x02
const EP_BULK_IN: u8 = 0x81;
const EP_BULK_OUT: u8 = 0x02;

pub struct UsbDevice {
    pub iface: Interface,
    pub kind: Programmer,
    /// Inter-command delay, sized by negotiated USB speed.
    pub ctrl_delay: Duration,
}

impl UsbDevice {
    fn recipient(&self) -> Recipient {
        if self.kind.uses_interface_recipient() {
            Recipient::Interface
        } else {
            Recipient::Device
        }
    }

    pub async fn ctrl_out(&self, req: UsbReq, data: u32, buf: Option<&[u8]>) -> Result<()> {
        self.ctrl_out_nodelay(req, data, buf).await?;
        tokio::time::sleep(self.ctrl_delay).await;
        Ok(())
    }

    /// ctrl_out without the trailing USB_DELAY — use when bulk_out follows immediately.
    pub async fn ctrl_out_nodelay(&self, req: UsbReq, data: u32, buf: Option<&[u8]>) -> Result<()> {
        let payload = buf.unwrap_or(&[]).to_vec();
        self.iface
            .control_out(
                ControlOut {
                    control_type: ControlType::Vendor,
                    recipient: self.recipient(),
                    request: req as u8,
                    value: ((data >> 16) & 0xFFFF) as u16,
                    index: (data & 0xFFFF) as u16,
                    data: &payload,
                },
                TIMEOUT,
            )
            .await
            .with_context(|| format!("ctrl_out {req:?} failed"))?;
        debug!("ctrl_out {req:?} data={data:#010x} ok");
        Ok(())
    }

    pub async fn ctrl_in(&self, req: UsbReq, data: u32, len: usize) -> Result<Vec<u8>> {
        let buf = self
            .iface
            .control_in(
                ControlIn {
                    control_type: ControlType::Vendor,
                    recipient: self.recipient(),
                    request: req as u8,
                    value: ((data >> 16) & 0xFFFF) as u16,
                    index: (data & 0xFFFF) as u16,
                    length: len as u16,
                },
                TIMEOUT,
            )
            .await
            .with_context(|| format!("ctrl_in {req:?} failed"))?;
        debug!("ctrl_in {req:?} -> {} bytes", buf.len());
        Ok(buf)
    }

    pub async fn abort(&self) {
        let _ = self.ctrl_out(UsbReq::Abort, 0, None).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    pub async fn bulk_in(&self, len: usize) -> Result<Vec<u8>> {
        let mut ep = self.iface.endpoint::<Bulk, In>(EP_BULK_IN)?;
        // usbfs requires buffer size to be a multiple of max packet size (512)
        let aligned = len.next_multiple_of(ep.max_packet_size());
        ep.submit(Buffer::new(aligned));
        let completion = ep.next_complete().await;
        if let Err(e) = completion.status {
            self.abort().await;
            bail!("bulk_in error: {e}");
        }
        Ok(completion.buffer[..completion.actual_len.min(len)].to_vec())
    }

    pub async fn bulk_out(&self, data: Vec<u8>) -> Result<()> {
        let mut ep = self.iface.endpoint::<Bulk, Out>(EP_BULK_OUT)?;
        ep.submit(data.into());
        let completion = ep.next_complete().await;
        if let Err(e) = completion.status {
            // Matches official fcusb USB.vb:401 — bulk transfer failure can stall the
            // EP; the firmware's ABORT request clears that state so subsequent
            // ctrl_out/bulk_in transfers don't fail with stale "device disconnected"
            // errors that are actually endpoint stalls.
            self.abort().await;
            bail!("bulk_out failed: {e}");
        }
        Ok(())
    }

    pub async fn echo(&self) -> Result<()> {
        let resp = self.ctrl_in(UsbReq::Echo, 0x454D4243, 4).await?;
        if resp != b"EMBC" {
            bail!("echo mismatch: {resp:?}");
        }
        Ok(())
    }

    pub async fn firmware_version(&self) -> Result<String> {
        let (_, ver) = self.version_raw().await?;
        Ok(ver)
    }

    /// Stored FPGA logic version (Mach1 MachXO2), request 0xC4 — 4 bytes,
    /// big-endian. Used to tell whether the CPLD already holds a given
    /// bitstream (it is non-volatile).
    pub async fn logic_version(&self) -> Result<u32> {
        let b = self.ctrl_in(UsbReq::LogicVersionGet, 0, 4).await?;
        if b.len() < 4 {
            bail!("short logic version response");
        }
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Persist the FPGA logic version (request 0xC5) so a future session can
    /// skip reprogramming a CPLD that already holds this bitstream.
    pub async fn logic_set_version(&self, ver: u32) -> Result<()> {
        self.ctrl_out(UsbReq::LogicVersionSet, ver, None).await
    }

    /// Raw VERSION response: `(board_type_byte, "X.YZ")`.
    /// b[0]=board type, b[1..3]=ASCII version e.g. '1','1','9' → "1.19".
    pub async fn version_raw(&self) -> Result<(u8, String)> {
        let b = self.ctrl_in(UsbReq::Version, 0, 4).await?;
        if b.len() < 4 {
            bail!("short version response");
        }
        Ok((b[0], format!("{}.{}{}", b[1] as char, b[2] as char, b[3] as char)))
    }

}
