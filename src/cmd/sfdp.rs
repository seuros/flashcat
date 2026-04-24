use anyhow::Result;

use crate::fpga::{self, Voltage};
use crate::spi::{self, SpiSpeed};
use crate::{setup, VoltageChoice};

pub async fn cmd_sfdp(vc: VoltageChoice, speed: SpiSpeed) -> Result<()> {
    // SFDP read is passive — probe at 3.3V if auto, otherwise use explicit voltage.
    // We don't need a chip DB entry; SFDP works on any JESD216-compliant chip.
    let voltage = match vc {
        VoltageChoice::Auto => Voltage::V3_3,
        VoltageChoice::Explicit(v) => v,
    };
    let dev = setup(voltage, speed).await?;
    let result = (async {
        let info = spi::read_sfdp(&dev).await?;

        println!("SFDP v{}.{}", info.sfdp_rev.0, info.sfdp_rev.1);
        println!("Size:       {} MB ({} bytes)", info.size_bytes / 1024 / 1024, info.size_bytes);
        println!("Page:       {} bytes", info.page_size);
        println!("DTR:        {}", if info.dtr_supported { "yes" } else { "no" });
        println!("Fast Read:  1-1-4={} 1-4-4={}",
            if info.fast_read_114 { "yes" } else { "no" },
            if info.fast_read_144 { "yes" } else { "no" },
        );
        println!("Erase types:");
        for et in &info.erase_types {
            let label = if et.size_bytes >= 65536 {
                format!("{} KB", et.size_bytes / 1024)
            } else {
                format!("{} bytes", et.size_bytes)
            };
            println!("  {:#04x}  {}", et.opcode, label);
        }
        Ok(())
    }).await;
    fpga::vcc_off(&dev).await.ok();
    result
}
