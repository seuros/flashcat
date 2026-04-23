use tokio_stream::StreamExt;
use nusb::hotplug::HotplugEvent;

use crate::fpga::{vcc_off, Voltage};
use crate::spi::SpiSpeed;
use crate::usb::{PID_CLASSIC, PID_MACH1, PID_PRO, VID_EC};
use crate::{setup, spi};

pub async fn cmd_watch(voltage: Voltage, speed: SpiSpeed) -> anyhow::Result<()> {
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

        let dev = match setup(voltage, speed).await {
            Ok(d) => d,
            Err(e) => { eprintln!("setup failed: {e}"); continue }
        };

        match spi::detect(&dev, voltage).await {
            Ok(Some(chip)) => {
                println!("Chip:  {}", chip.name);
                println!("Size:  {} MB ({} bytes)", chip.size_bytes / 1024 / 1024, chip.size_bytes);
                println!("Erase: {} bytes | Page: {} bytes | Addr: {}-byte",
                    chip.erase_size, chip.page_size, chip.addr_bytes);
            }
            Ok(None) => println!("No chip detected"),
            Err(e)   => eprintln!("detect failed: {e}"),
        }

        // Cut VCC so a different-voltage chip can be swapped safely
        if let Err(e) = vcc_off(&dev).await {
            tracing::debug!("vcc_off: {e}");
        }
    }

    Ok(())
}
