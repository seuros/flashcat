use anyhow::Result;

mod device;
mod requests;

pub use device::UsbDevice;
pub use requests::UsbReq;

pub const VID_EC: u16 = 0x16C0;
pub const PID_CLASSIC: u16 = 0x05DE;
pub const PID_PRO: u16 = 0x05E0;
pub const PID_MACH1: u16 = 0x05E1;

/// Number of attempts to find and open the device. The previous session's
/// LogicOff (vcc_off) can briefly knock the firmware off the bus on some
/// hardware; we sleep and retry rather than failing the user immediately.
const CONNECT_ATTEMPTS: u32 = 6;
const CONNECT_BACKOFF_MS: u64 = 150;

pub async fn connect() -> Result<UsbDevice> {
    use crate::programmer::Programmer;
    use nusb::Speed;
    use std::time::Duration;

    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..CONNECT_ATTEMPTS {
        let maybe_di = nusb::list_devices()
            .await?
            .find(|d| {
                d.vendor_id() == VID_EC
                    && matches!(d.product_id(), p if p == PID_CLASSIC || p == PID_PRO || p == PID_MACH1)
            });

        let Some(di) = maybe_di else {
            // Programmer not on the bus — could be mid-reenumeration from the
            // previous session's LogicOff. Back off and retry.
            last_err = Some(anyhow::anyhow!(
                "FlashcatUSB not found — check USB connection and udev rules"
            ));
            tokio::time::sleep(Duration::from_millis(CONNECT_BACKOFF_MS)).await;
            continue;
        };

        let kind = match di.product_id() {
            PID_PRO    => Programmer::Pro5,
            PID_MACH1  => Programmer::Mach1,
            _          => Programmer::Classic,
        };

        // Derive inter-command delay from the kernel-reported negotiated speed.
        let ctrl_delay = match di.speed() {
            Some(Speed::High) | Some(Speed::Super) | Some(Speed::SuperPlus) => Duration::from_millis(5),
            _ => Duration::from_millis(5), // Full/Low/unknown — safe default
        };

        let attempt_result: Result<UsbDevice> = async {
            let device = di.open().await?;
            // Mirrors official USB.vb:165 OpenUsbDevice — explicit configuration
            // selection before claiming. Some firmware revisions leave config 0
            // selected after LogicOff, which would refuse interface claims.
            // Errors here are best-effort; not all platforms support it.
            let _ = device.set_configuration(1).await;
            let iface = device.claim_interface(0).await?;

            // Pro/Mach1: bulk endpoints live in alternate setting 1
            if kind.has_fpga() {
                iface.set_alt_setting(1).await?;
            }
            Ok(UsbDevice { iface, kind, ctrl_delay })
        }.await;

        match attempt_result {
            Ok(dev) => return Ok(dev),
            Err(e) => {
                tracing::debug!(
                    "connect attempt {}/{}: {e}",
                    attempt + 1, CONNECT_ATTEMPTS
                );
                last_err = Some(e);
                tokio::time::sleep(Duration::from_millis(CONNECT_BACKOFF_MS)).await;
            }
        }
    }

    Err(last_err
        .unwrap_or_else(|| anyhow::anyhow!("FlashcatUSB connect failed after retries")))
}
