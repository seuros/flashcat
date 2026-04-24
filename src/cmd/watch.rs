use tokio_stream::StreamExt;
use nusb::hotplug::HotplugEvent;

use crate::fpga::vcc_off;
use crate::spi::SpiSpeed;
use crate::usb::{PID_CLASSIC, PID_MACH1, PID_PRO, VID_EC};
use crate::{prepare, VoltageChoice};

pub async fn cmd_watch(vc: VoltageChoice, speed: SpiSpeed) -> anyhow::Result<()> {
    println!("Watching for FlashcatUSB — press Ctrl-C to stop");

    let mut stream = nusb::watch_devices()?;

    while let Some(event) = stream.next().await {
        let HotplugEvent::Connected(di) = event else { continue };

        if di.vendor_id() != VID_EC
            || !matches!(di.product_id(), p if p == PID_CLASSIC || p == PID_PRO || p == PID_MACH1)
        {
            continue;
        }

        println!("\nFlashcatUSB connected (PID {:#06x})", di.product_id());

        // VoltageChoice is not Clone — rebuild from the same variant each iteration.
        // watch is always called with Auto (the default); explicit variants are unlikely
        // but still handled correctly since we reconstruct from the outer vc each time.
        let iter_vc = match &vc {
            VoltageChoice::Auto => VoltageChoice::Auto,
            VoltageChoice::Explicit(v) => VoltageChoice::Explicit(*v),
        };

        match prepare(iter_vc, speed).await {
            Ok((dev, chip, voltage)) => {
                println!("Chip:    {}", chip.name);
                println!("Size:    {} MB ({} bytes)", chip.size_bytes / 1024 / 1024, chip.size_bytes);
                println!("Erase:   {} bytes | Page: {} bytes | Addr: {}-byte",
                    chip.erase_size, chip.page_size, chip.addr_bytes);
                println!("Voltage: {:?}", voltage);

                if let Err(e) = vcc_off(&dev).await {
                    tracing::debug!("vcc_off: {e}");
                }
            }
            Err(e) if e.to_string().contains("no chip detected") => println!("No chip detected"),
            Err(e) => eprintln!("detect failed: {e}"),
        }
    }

    Ok(())
}
