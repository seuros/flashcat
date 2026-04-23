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
const USB_DELAY: Duration = Duration::from_millis(25);

// EP1 IN = 0x81, EP2 OUT = 0x02
const EP_BULK_IN: u8 = 0x81;
const EP_BULK_OUT: u8 = 0x02;

pub struct UsbDevice {
    pub iface: Interface,
    pub kind: Programmer,
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
        tokio::time::sleep(USB_DELAY).await;
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
        let aligned = len.next_multiple_of(ep.max_packet_size() as usize);
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
        completion.status.context("bulk_out failed")?;
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
        let b = self.ctrl_in(UsbReq::Version, 0, 4).await?;
        if b.len() < 4 {
            bail!("short version response");
        }
        // b[0]=board type, b[1..3]=ASCII version e.g. '1','1','9' → "1.19"
        Ok(format!("{}.{}{}", b[1] as char, b[2] as char, b[3] as char))
    }
}
